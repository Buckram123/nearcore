use std::collections::{HashMap, HashSet};

use std::sync::Arc;
use std::time::{Duration as TimeDuration, Instant};

use borsh::BorshSerialize;
use chrono::Duration;
use itertools::Itertools;
use near_primitives::time::Clock;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use tracing::{debug, error, info, warn};

use near_chain_primitives::error::{BlockKnownError, Error, ErrorKind, LogTransientStorageError};
use near_primitives::block::{genesis_chunks, Tip};
use near_primitives::challenge::{
    BlockDoubleSign, Challenge, ChallengeBody, ChallengesResult, ChunkProofs, ChunkState,
    MaybeEncodedShardChunk, SlashedValidator,
};
use near_primitives::checked_feature;
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::merkle::{
    combine_hash, merklize, verify_path, Direction, MerklePath, MerklePathItem,
};
use near_primitives::receipt::Receipt;
use near_primitives::sharding::{
    ChunkHash, ChunkHashHeight, ReceiptList, ReceiptProof, ShardChunk, ShardChunkHeader, ShardInfo,
    ShardProof, StateSyncInfo,
};
use near_primitives::state_part::PartId;
use near_primitives::syncing::{
    get_num_state_parts, ReceiptProofResponse, RootProof, ShardStateSyncResponseHeader,
    ShardStateSyncResponseHeaderV1, ShardStateSyncResponseHeaderV2, StateHeaderKey, StatePartKey,
};
use near_primitives::transaction::ExecutionOutcomeWithIdAndProof;
use near_primitives::types::chunk_extra::ChunkExtra;
use near_primitives::types::{
    AccountId, Balance, BlockExtra, BlockHeight, BlockHeightDelta, EpochId, Gas, MerkleHash,
    NumBlocks, NumShards, ShardId, StateChangesForSplitStates, StateRoot,
};
use near_primitives::unwrap_or_return;
use near_primitives::utils::MaybeValidated;
use near_primitives::views::{
    ExecutionOutcomeWithIdView, ExecutionStatusView, FinalExecutionOutcomeView,
    FinalExecutionOutcomeWithReceiptView, FinalExecutionStatus, LightClientBlockView,
    SignedTransactionView,
};
use near_store::{ColState, ColStateHeaders, ColStateParts, ShardTries, StoreUpdate};

use near_primitives::state_record::StateRecord;

use crate::blocks_delay_tracker::BlocksDelayTracker;
use crate::crypto_hash_timer::CryptoHashTimer;
use crate::lightclient::get_epoch_block_producers_view;
use crate::migrations::check_if_block_is_first_with_chunk_of_version;
use crate::missing_chunks::{BlockLike, MissingChunksPool};
use crate::store::{ChainStore, ChainStoreAccess, ChainStoreUpdate, GCMode, SavedStoreUpdate};
use crate::types::{
    AcceptedBlock, ApplySplitStateResult, ApplySplitStateResultOrStateChanges,
    ApplyTransactionResult, Block, BlockEconomicsConfig, BlockHeader, BlockHeaderInfo, BlockStatus,
    ChainGenesis, Provenance, RuntimeAdapter,
};
use crate::validate::{
    validate_challenge, validate_chunk_proofs, validate_chunk_with_chunk_extra,
    validate_transactions_order,
};
use crate::{byzantine_assert, create_light_client_block_view, Doomslug};
use crate::{metrics, DoomslugThresholdMode};
use actix::Message;
use delay_detector::DelayDetector;
use near_primitives::shard_layout::{
    account_id_to_shard_id, account_id_to_shard_uid, ShardLayout, ShardUId,
};
use rayon::iter::{IntoParallelIterator, ParallelIterator};

/// Maximum number of orphans chain can store.
pub const MAX_ORPHAN_SIZE: usize = 1024;

/// Maximum age of orhpan to store in the chain.
const MAX_ORPHAN_AGE_SECS: u64 = 300;

// Number of orphan ancestors should be checked to request chunks
// Orphans for which we will request for missing chunks must satisfy,
// its NUM_ORPHAN_ANCESTORS_CHECK'th ancestor has been accepted
pub const NUM_ORPHAN_ANCESTORS_CHECK: u64 = 3;

// Maximum number of orphans that we can request missing chunks
// Note that if there are no forks, the maximum number of orphans we would
// request missing chunks will not exceed NUM_ORPHAN_ANCESTORS_CHECK,
// this number only adds another restriction when there are multiple forks.
// It should almost never be hit
const MAX_ORPHAN_MISSING_CHUNKS: usize = 5;

/// 10000 years in seconds. Big constant for sandbox to allow time traveling.
#[cfg(feature = "sandbox")]
const ACCEPTABLE_TIME_DIFFERENCE: i64 = 60 * 60 * 24 * 365 * 10000;

/// Refuse blocks more than this many block intervals in the future (as in bitcoin).
#[cfg(not(feature = "sandbox"))]
const ACCEPTABLE_TIME_DIFFERENCE: i64 = 12 * 10;

/// Over this block height delta in advance if we are not chunk producer - route tx to upcoming validators.
pub const TX_ROUTING_HEIGHT_HORIZON: BlockHeightDelta = 4;

/// Private constant for 1 NEAR (copy from near/config.rs) used for reporting.
const NEAR_BASE: Balance = 1_000_000_000_000_000_000_000_000;

/// Number of epochs for which we keep store data
pub const NUM_EPOCHS_TO_KEEP_STORE_DATA: u64 = 5;

/// Maximum number of height to go through at each step when cleaning forks during garbage collection.
const GC_FORK_CLEAN_STEP: u64 = 1000;

/// apply_chunks may be called in two code paths, through process_block or through catchup_blocks
/// When it is called through process_block, it is possible that the shard state for the next epoch
/// has not been caught up yet, thus the two modes IsCaughtUp and NotCaughtUp.
/// CatchingUp is for when apply_chunks is called through catchup_blocks, this is to catch up the
/// shard states for the next epoch
#[derive(Eq, PartialEq)]
enum ApplyChunksMode {
    IsCaughtUp,
    CatchingUp,
    NotCaughtUp,
}

/// Orphan is a block whose previous block is not accepted (in store) yet.
/// Therefore, they are not ready to be processed yet.
/// We save these blocks in an in-memory orphan pool to be processed later
/// after their previous block is accepted.
pub struct Orphan {
    block: MaybeValidated<Block>,
    provenance: Provenance,
    added: Instant,
}

impl BlockLike for Orphan {
    fn hash(&self) -> CryptoHash {
        *self.block.hash()
    }

    fn height(&self) -> u64 {
        self.block.header().height()
    }
}

impl Orphan {
    fn prev_hash(&self) -> &CryptoHash {
        self.block.header().prev_hash()
    }
}

/// OrphanBlockPool stores information of all orphans that are waiting to be processed
/// A block is added to the orphan pool when process_block failed because the block is an orphan
/// A block is removed from the pool if
/// 1) it is ready to be processed
/// or
/// 2) size of the pool exceeds MAX_ORPHAN_SIZE and the orphan was added a long time ago
///    or the height is high
pub struct OrphanBlockPool {
    /// A map from block hash to a orphan block
    orphans: HashMap<CryptoHash, Orphan>,
    /// A set that contains all orphans for which we have requested missing chunks for them
    /// An orphan can be added to this set when it was first added to the pool, or later
    /// when certain requirements are satisfied (see check_orphans)
    /// It can only be removed from this set when the orphan is removed from the pool
    orphans_requested_missing_chunks: HashSet<CryptoHash>,
    /// A map from block heights to orphan blocks at the height
    /// It's used to evict orphans when the pool is saturated
    height_idx: HashMap<BlockHeight, Vec<CryptoHash>>,
    /// A map from block hashes to orphan blocks whose prev block is the block
    /// It's used to check which orphan blocks are ready to be processed when a block is accepted
    prev_hash_idx: HashMap<CryptoHash, Vec<CryptoHash>>,
    /// number of orphans that were evicted
    evicted: usize,
}

impl OrphanBlockPool {
    pub fn new() -> OrphanBlockPool {
        OrphanBlockPool {
            orphans: HashMap::default(),
            orphans_requested_missing_chunks: HashSet::default(),
            height_idx: HashMap::default(),
            prev_hash_idx: HashMap::default(),
            evicted: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.orphans.len()
    }

    fn len_evicted(&self) -> usize {
        self.evicted
    }

    /// Add a block to the orphan pool
    /// `requested_missing_chunks`: whether missing chunks has been requested for the orphan
    fn add(&mut self, orphan: Orphan, requested_missing_chunks: bool) {
        let block_hash = *orphan.block.hash();
        let height_hashes = self.height_idx.entry(orphan.block.header().height()).or_default();
        height_hashes.push(*orphan.block.hash());
        let prev_hash_entries =
            self.prev_hash_idx.entry(*orphan.block.header().prev_hash()).or_default();
        prev_hash_entries.push(block_hash);
        self.orphans.insert(block_hash, orphan);
        if requested_missing_chunks {
            self.orphans_requested_missing_chunks.insert(block_hash);
        }

        if self.orphans.len() > MAX_ORPHAN_SIZE {
            let old_len = self.orphans.len();

            let mut removed_hashes: HashSet<CryptoHash> = HashSet::default();
            self.orphans.retain(|_, ref mut x| {
                let keep = x.added.elapsed() < TimeDuration::from_secs(MAX_ORPHAN_AGE_SECS);
                if !keep {
                    removed_hashes.insert(*x.block.hash());
                }
                keep
            });
            let mut heights = self.height_idx.keys().cloned().collect::<Vec<u64>>();
            heights.sort_unstable();
            for h in heights.iter().rev() {
                if let Some(hash) = self.height_idx.remove(h) {
                    for h in hash {
                        let _ = self.orphans.remove(&h);
                        removed_hashes.insert(h);
                    }
                }
                if self.orphans.len() < MAX_ORPHAN_SIZE {
                    break;
                }
            }
            self.height_idx.retain(|_, ref mut xs| xs.iter().any(|x| !removed_hashes.contains(x)));
            self.prev_hash_idx
                .retain(|_, ref mut xs| xs.iter().any(|x| !removed_hashes.contains(x)));
            self.orphans_requested_missing_chunks.retain(|x| !removed_hashes.contains(x));

            self.evicted += old_len - self.orphans.len();
        }
    }

    pub fn contains(&self, hash: &CryptoHash) -> bool {
        self.orphans.contains_key(hash)
    }

    pub fn get(&self, hash: &CryptoHash) -> Option<&Orphan> {
        self.orphans.get(hash)
    }

    // Iterates over existing orphans.
    pub fn map(&self, orphan_fn: &mut dyn FnMut(&CryptoHash, &Block, &Instant)) {
        self.orphans
            .iter()
            .map(|it| orphan_fn(it.0, it.1.block.get_inner(), &it.1.added))
            .collect_vec();
    }

    /// Remove all orphans in the pool that can be "adopted" by block `prev_hash`, i.e., children
    /// of `prev_hash` and return the list.
    /// This function is called when `prev_hash` is accepted, thus its children can be removed
    /// from the orphan pool and be processed.
    pub fn remove_by_prev_hash(&mut self, prev_hash: CryptoHash) -> Option<Vec<Orphan>> {
        let mut removed_hashes: HashSet<CryptoHash> = HashSet::default();
        let ret = self.prev_hash_idx.remove(&prev_hash).map(|hs| {
            hs.iter()
                .filter_map(|h| {
                    removed_hashes.insert(*h);
                    self.orphans_requested_missing_chunks.remove(h);
                    self.orphans.remove(h)
                })
                .collect()
        });

        self.height_idx.retain(|_, ref mut xs| xs.iter().any(|x| !removed_hashes.contains(x)));

        ret
    }

    /// Return a list of orphans that are among the `target_depth` immediate descendants of
    /// the block `parent_hash`
    pub fn get_orphans_within_depth(
        &self,
        parent_hash: CryptoHash,
        target_depth: u64,
    ) -> Vec<CryptoHash> {
        let mut _visited = HashSet::new();

        let mut res = vec![];
        let mut queue = vec![(parent_hash, 0)];
        while let Some((prev_hash, depth)) = queue.pop() {
            if depth == target_depth {
                break;
            }
            if let Some(block_hashes) = self.prev_hash_idx.get(&prev_hash) {
                for hash in block_hashes {
                    queue.push((*hash, depth + 1));
                    res.push(*hash);
                    // there should be no loop
                    debug_assert!(_visited.insert(*hash));
                }
            }

            // probably something serious went wrong here because there shouldn't be so many forks
            assert!(
                res.len() <= 100 * target_depth as usize,
                "found too many orphans {:?}, probably something is wrong with the chain",
                res
            );
        }
        res
    }

    /// Returns true if the block has not been requested yet and the number of orphans
    /// for which we have requested missing chunks have not exceeded MAX_ORPHAN_MISSING_CHUNKS
    fn can_request_missing_chunks_for_orphan(&self, block_hash: &CryptoHash) -> bool {
        self.orphans_requested_missing_chunks.len() < MAX_ORPHAN_MISSING_CHUNKS
            && !self.orphans_requested_missing_chunks.contains(block_hash)
    }

    fn mark_missing_chunks_requested_for_orphan(&mut self, block_hash: CryptoHash) {
        self.orphans_requested_missing_chunks.insert(block_hash);
    }
}

/// Contains information for missing chunks in a block
pub struct BlockMissingChunks {
    /// previous block hash
    pub prev_hash: CryptoHash,
    pub missing_chunks: Vec<ShardChunkHeader>,
    pub block_hash: CryptoHash,
}

/// Contains information needed to request chunks for orphans
/// Fields will be used as arguments for `request_chunks_for_orphan`
pub struct OrphanMissingChunks {
    pub missing_chunks: Vec<ShardChunkHeader>,
    /// epoch id for the block that has missing chunks
    pub epoch_id: EpochId,
    /// hash of an ancestor block of the block that has missing chunks
    /// this is used as an argument for `request_chunks_for_orphan`
    /// see comments in `request_chunks_for_orphan` for what `ancestor_hash` is used for
    pub ancestor_hash: CryptoHash,
    // Block hash that was requesting this chunk.
    pub requestor_block_hash: CryptoHash,
}

/// Provides view on the current chain state
/// Both Chain and ChainUpdate implement this trait,
/// to avoid duplicate functions
pub trait ChainAccess {
    fn orphans(&self) -> &OrphanBlockPool;
    fn blocks_with_missing_chunks(&self) -> &MissingChunksPool<Orphan>;
    fn chain_store(&self) -> &dyn ChainStoreAccess;
}

/// Check if block header is known
/// Returns Err(Error) if any error occurs when checking store
///         Ok(Err(BlockKnownError)) if the block header is known
///         Ok(Ok()) otherwise
pub fn check_header_known(
    chain: &dyn ChainAccess,
    header: &BlockHeader,
) -> Result<Result<(), BlockKnownError>, Error> {
    let header_head = chain.chain_store().header_head()?;
    if header.hash() == &header_head.last_block_hash
        || header.hash() == &header_head.prev_block_hash
    {
        return Ok(Err(BlockKnownError::KnownInHeader));
    }
    check_known_store(chain, header.hash())
}

/// Check if this block is in the store already.
/// Returns Err(Error) if any error occurs when checking store
///         Ok(Err(BlockKnownError)) if the block is in the store
///         Ok(Ok()) otherwise
fn check_known_store(
    chain: &dyn ChainAccess,
    block_hash: &CryptoHash,
) -> Result<Result<(), BlockKnownError>, Error> {
    if chain.chain_store().block_exists(block_hash)? {
        Ok(Err(BlockKnownError::KnownInStore))
    } else {
        // Not yet processed this block, we can proceed.
        Ok(Ok(()))
    }
}

/// Check if block is known: head, orphan or in store.
/// Returns Err(Error) if any error occurs when checking store
///         Ok(Err(BlockKnownError)) if the block is known
///         Ok(Ok()) otherwise
pub fn check_known(
    chain: &dyn ChainAccess,
    block_hash: &CryptoHash,
) -> Result<Result<(), BlockKnownError>, Error> {
    let head = chain.chain_store().head()?;
    // Quick in-memory check for fast-reject any block handled recently.
    if block_hash == &head.last_block_hash || block_hash == &head.prev_block_hash {
        return Ok(Err(BlockKnownError::KnownInHead));
    }
    // Check if this block is in the set of known orphans.
    if chain.orphans().contains(block_hash) {
        return Ok(Err(BlockKnownError::KnownInOrphan));
    }
    if chain.blocks_with_missing_chunks().contains(block_hash) {
        return Ok(Err(BlockKnownError::KnownInMissingChunks));
    }
    check_known_store(chain, block_hash)
}

/// Facade to the blockchain block processing and storage.
/// Provides current view on the state according to the chain state.
pub struct Chain {
    store: ChainStore,
    pub runtime_adapter: Arc<dyn RuntimeAdapter>,
    orphans: OrphanBlockPool,
    pub blocks_with_missing_chunks: MissingChunksPool<Orphan>,
    genesis: Block,
    pub transaction_validity_period: NumBlocks,
    pub epoch_length: BlockHeightDelta,
    /// Block economics, relevant to changes when new block must be produced.
    pub block_economics_config: BlockEconomicsConfig,
    pub doomslug_threshold_mode: DoomslugThresholdMode,
    pending_states_to_patch: Option<Vec<StateRecord>>,
    pub blocks_delay_tracker: BlocksDelayTracker,
}

impl ChainAccess for Chain {
    fn orphans(&self) -> &OrphanBlockPool {
        &self.orphans
    }

    fn blocks_with_missing_chunks(&self) -> &MissingChunksPool<Orphan> {
        &self.blocks_with_missing_chunks
    }

    fn chain_store(&self) -> &dyn ChainStoreAccess {
        &self.store
    }
}

impl Chain {
    pub fn new_for_view_client(
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        chain_genesis: &ChainGenesis,
        doomslug_threshold_mode: DoomslugThresholdMode,
    ) -> Result<Chain, Error> {
        let (store, state_roots) = runtime_adapter.genesis_state();
        let store = ChainStore::new(store, chain_genesis.height);
        let genesis_chunks = genesis_chunks(
            state_roots,
            runtime_adapter.num_shards(&EpochId::default())?,
            chain_genesis.gas_limit,
            chain_genesis.height,
            chain_genesis.protocol_version,
        );
        let genesis = Block::genesis(
            chain_genesis.protocol_version,
            genesis_chunks.into_iter().map(|chunk| chunk.take_header()).collect(),
            chain_genesis.time,
            chain_genesis.height,
            chain_genesis.min_gas_price,
            chain_genesis.total_supply,
            Chain::compute_bp_hash(
                &*runtime_adapter,
                EpochId::default(),
                EpochId::default(),
                &CryptoHash::default(),
            )?,
        );
        Ok(Chain {
            store,
            runtime_adapter,
            orphans: OrphanBlockPool::new(),
            blocks_with_missing_chunks: MissingChunksPool::new(),
            genesis: genesis,
            transaction_validity_period: chain_genesis.transaction_validity_period,
            epoch_length: chain_genesis.epoch_length,
            block_economics_config: BlockEconomicsConfig::from(chain_genesis),
            doomslug_threshold_mode,
            pending_states_to_patch: None,
            blocks_delay_tracker: BlocksDelayTracker::default(),
        })
    }

    pub fn new(
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        chain_genesis: &ChainGenesis,
        doomslug_threshold_mode: DoomslugThresholdMode,
    ) -> Result<Chain, Error> {
        // Get runtime initial state and create genesis block out of it.
        let (store, state_roots) = runtime_adapter.genesis_state();
        let mut store = ChainStore::new(store, chain_genesis.height);
        let genesis_chunks = genesis_chunks(
            state_roots.clone(),
            runtime_adapter.num_shards(&EpochId::default())?,
            chain_genesis.gas_limit,
            chain_genesis.height,
            chain_genesis.protocol_version,
        );
        let genesis = Block::genesis(
            chain_genesis.protocol_version,
            genesis_chunks.iter().map(|chunk| chunk.cloned_header()).collect(),
            chain_genesis.time,
            chain_genesis.height,
            chain_genesis.min_gas_price,
            chain_genesis.total_supply,
            Chain::compute_bp_hash(
                &*runtime_adapter,
                EpochId::default(),
                EpochId::default(),
                &CryptoHash::default(),
            )?,
        );

        // Check if we have a head in the store, otherwise pick genesis block.
        let mut store_update = store.store_update();
        let head_res = store_update.head();
        let head: Tip;
        match head_res {
            Ok(h) => {
                head = h;

                // Check that genesis in the store is the same as genesis given in the config.
                let genesis_hash = store_update.get_block_hash_by_height(chain_genesis.height)?;
                if &genesis_hash != genesis.hash() {
                    return Err(ErrorKind::Other(format!(
                        "Genesis mismatch between storage and config: {:?} vs {:?}",
                        genesis_hash,
                        genesis.hash()
                    ))
                    .into());
                }

                // Check we have the header corresponding to the header_head.
                let header_head = store_update.header_head()?;
                if store_update.get_block_header(&header_head.last_block_hash).is_err() {
                    // Reset header head and "sync" head to be consistent with current block head.
                    store_update.save_header_head_if_not_challenged(&head)?;
                }
                // TODO: perform validation that latest state in runtime matches the stored chain.
            }
            Err(err) => match err.kind() {
                ErrorKind::DBNotFoundErr(_) => {
                    for chunk in genesis_chunks {
                        store_update.save_chunk(chunk.clone());
                    }
                    store_update.merge(runtime_adapter.add_validator_proposals(
                        BlockHeaderInfo::new(
                            genesis.header(),
                            // genesis height is considered final
                            chain_genesis.height,
                        ),
                    )?);
                    store_update.save_block_header(genesis.header().clone())?;
                    store_update.save_block(genesis.clone());
                    store_update
                        .save_block_extra(genesis.hash(), BlockExtra { challenges_result: vec![] });

                    for (chunk_header, state_root) in
                        genesis.chunks().iter().zip(state_roots.iter())
                    {
                        store_update.save_chunk_extra(
                            genesis.hash(),
                            &runtime_adapter
                                .shard_id_to_uid(chunk_header.shard_id(), &EpochId::default())?,
                            ChunkExtra::new(
                                state_root,
                                CryptoHash::default(),
                                vec![],
                                0,
                                chain_genesis.gas_limit,
                                0,
                            ),
                        );
                    }

                    head = Tip::from_header(genesis.header());
                    store_update.save_head(&head)?;
                    store_update.save_final_head(&head)?;

                    info!(target: "chain", "Init: saved genesis: {:?} / {:?}", genesis.hash(), state_roots);
                }
                e => return Err(e.into()),
            },
        }
        store_update.commit()?;

        info!(target: "chain", "Init: head @ {} [{}]", head.height, head.last_block_hash);

        Ok(Chain {
            store,
            runtime_adapter,
            orphans: OrphanBlockPool::new(),
            blocks_with_missing_chunks: MissingChunksPool::new(),
            genesis: genesis.clone(),
            transaction_validity_period: chain_genesis.transaction_validity_period,
            epoch_length: chain_genesis.epoch_length,
            block_economics_config: BlockEconomicsConfig::from(chain_genesis),
            doomslug_threshold_mode,
            pending_states_to_patch: None,
            blocks_delay_tracker: BlocksDelayTracker::default(),
        })
    }

    #[cfg(feature = "test_features")]
    pub fn adv_disable_doomslug(&mut self) {
        self.doomslug_threshold_mode = DoomslugThresholdMode::NoApprovals
    }

    pub fn compute_collection_hash<T: BorshSerialize>(elems: Vec<T>) -> Result<CryptoHash, Error> {
        Ok(hash(&elems.try_to_vec()?))
    }

    pub fn compute_bp_hash(
        runtime_adapter: &dyn RuntimeAdapter,
        epoch_id: EpochId,
        prev_epoch_id: EpochId,
        last_known_hash: &CryptoHash,
    ) -> Result<CryptoHash, Error> {
        let bps = runtime_adapter.get_epoch_block_producers_ordered(&epoch_id, last_known_hash)?;
        let protocol_version = runtime_adapter.get_epoch_protocol_version(&prev_epoch_id)?;
        if checked_feature!("stable", BlockHeaderV3, protocol_version) {
            let validator_stakes = bps.into_iter().map(|(bp, _)| bp).collect();
            Chain::compute_collection_hash(validator_stakes)
        } else {
            let validator_stakes = bps.into_iter().map(|(bp, _)| bp.into_v1()).collect();
            Chain::compute_collection_hash(validator_stakes)
        }
    }

    /// Creates a light client block for the last final block from perspective of some other block
    ///
    /// # Arguments
    ///  * `header` - the last finalized block seen from `header` (not pushed back) will be used to
    ///               compute the light client block
    pub fn create_light_client_block(
        header: &BlockHeader,
        runtime_adapter: &dyn RuntimeAdapter,
        chain_store: &mut dyn ChainStoreAccess,
    ) -> Result<LightClientBlockView, Error> {
        let final_block_header = {
            let ret = chain_store.get_block_header(header.last_final_block())?.clone();
            let two_ahead = chain_store.get_header_by_height(ret.height() + 2)?;
            if two_ahead.epoch_id() != ret.epoch_id() {
                let one_ahead = chain_store.get_header_by_height(ret.height() + 1)?;
                if one_ahead.epoch_id() != ret.epoch_id() {
                    let new_final_hash = *ret.last_final_block();
                    chain_store.get_block_header(&new_final_hash)?.clone()
                } else {
                    let new_final_hash = *one_ahead.last_final_block();
                    chain_store.get_block_header(&new_final_hash)?.clone()
                }
            } else {
                ret
            }
        };

        let next_block_producers = get_epoch_block_producers_view(
            final_block_header.next_epoch_id(),
            header.prev_hash(),
            runtime_adapter,
        )?;

        create_light_client_block_view(&final_block_header, chain_store, Some(next_block_producers))
    }

    pub fn save_block(&mut self, block: MaybeValidated<Block>) -> Result<(), Error> {
        if self.store.get_block(block.hash()).is_ok() {
            return Ok(());
        }
        if let Err(e) = self.validate_block(&block) {
            byzantine_assert!(false);
            return Err(e.into());
        }

        let mut chain_store_update = ChainStoreUpdate::new(&mut self.store);

        chain_store_update.save_block(block.into_inner());
        // We don't need to increase refcount for `prev_hash` at this point
        // because this is the block before State Sync.

        chain_store_update.commit()?;
        Ok(())
    }

    pub fn save_orphan(
        &mut self,
        block: MaybeValidated<Block>,
        requested_missing_chunks: bool,
    ) -> Result<(), Error> {
        if self.orphans.contains(block.hash()) {
            return Ok(());
        }
        if let Err(e) = self.validate_block(&block) {
            byzantine_assert!(false);
            return Err(e.into());
        }
        self.orphans.add(
            Orphan { block, provenance: Provenance::NONE, added: Clock::instant() },
            requested_missing_chunks,
        );
        Ok(())
    }

    fn save_block_height_processed(&mut self, block_height: BlockHeight) -> Result<(), Error> {
        let mut chain_store_update = ChainStoreUpdate::new(&mut self.store);
        if !chain_store_update.is_height_processed(block_height)? {
            chain_store_update.save_block_height_processed(block_height);
        }
        chain_store_update.commit()?;
        Ok(())
    }

    // GC CONTRACT
    // ===
    //
    // Prerequisites, guaranteed by the System:
    // 1. Genesis block is available and should not be removed by GC.
    // 2. No block in storage except Genesis has height lower or equal to `genesis_height`.
    // 3. There is known lowest block height (Tail) came from Genesis or State Sync.
    //    a. Tail is always on the Canonical Chain.
    //    b. Only one Tail exists.
    //    c. Tail's height is higher than or equal to `genesis_height`,
    // 4. There is a known highest block height (Head).
    //    a. Head is always on the Canonical Chain.
    // 5. All blocks in the storage have heights in range [Tail; Head].
    //    a. All forks end up on height of Head or lower.
    // 6. If block A is ancestor of block B, height of A is strictly less then height of B.
    // 7. (Property 1). A block with the lowest height among all the blocks at which the fork has started,
    //    i.e. all the blocks with the outgoing degree 2 or more,
    //    has the least height among all blocks on the fork.
    // 8. (Property 2). The oldest block where the fork happened is never affected
    //    by Canonical Chain Switching and always stays on Canonical Chain.
    //
    // Overall:
    // 1. GC procedure is handled by `clear_data()` function.
    // 2. `clear_data()` runs GC process for all blocks from the Tail to GC Stop Height provided by Epoch Manager.
    // 3. `clear_data()` executes separately:
    //    a. Forks Clearing runs for each height from Tail up to GC Stop Height.
    //    b. Canonical Chain Clearing from (Tail + 1) up to GC Stop Height.
    // 4. Before actual clearing is started, Block Reference Map should be built.
    // 5. `clear_data()` executes every time when block at new height is added.
    // 6. In case of State Sync, State Sync Clearing happens.
    //
    // Forks Clearing:
    // 1. Any fork which ends up on height `height` INCLUSIVELY and earlier will be completely deleted
    //    from the Store with all its ancestors up to the ancestor block where fork is happened
    //    EXCLUDING the ancestor block where fork is happened.
    // 2. The oldest ancestor block always remains on the Canonical Chain by property 2.
    // 3. All forks which end up on height `height + 1` and further are protected from deletion and
    //    no their ancestor will be deleted (even with lowest heights).
    // 4. `clear_forks_data()` handles forks clearing for fixed height `height`.
    //
    // Canonical Chain Clearing:
    // 1. Blocks on the Canonical Chain with the only descendant (if no forks started from them)
    //    are unlocked for Canonical Chain Clearing.
    // 2. If Forks Clearing ended up on the Canonical Chain, the block may be unlocked
    //    for the Canonical Chain Clearing. There is no other reason to unlock the block exists.
    // 3. All the unlocked blocks will be completely deleted
    //    from the Tail up to GC Stop Height EXCLUSIVELY.
    // 4. (Property 3, GC invariant). Tail can be shifted safely to the height of the
    //    earliest existing block. There is always only one Tail (based on property 1)
    //    and it's always on the Canonical Chain (based on property 2).
    //
    // Example:
    //
    // height: 101   102   103   104
    // --------[A]---[B]---[C]---[D]
    //          \     \
    //           \     \---[E]
    //            \
    //             \-[F]---[G]
    //
    // 1. Let's define clearing height = 102. It this case fork A-F-G is protected from deletion
    //    because of G which is on height 103. Nothing will be deleted.
    // 2. Let's define clearing height = 103. It this case Fork Clearing will be executed for A
    //    to delete blocks G and F, then Fork Clearing will be executed for B to delete block E.
    //    Then Canonical Chain Clearing will delete blocks A and B as unlocked.
    //    Block C is the only block of height 103 remains on the Canonical Chain (invariant).
    //
    // State Sync Clearing:
    // 1. Executing State Sync means that no data in the storage is useful for block processing
    //    and should be removed completely.
    // 2. The Tail should be set to the block preceding Sync Block.
    // 3. All the data preceding new Tail is deleted in State Sync Clearing
    //    and the Trie is updated with having only Genesis data.
    // 4. State Sync Clearing happens in `reset_data_pre_state_sync()`.
    //
    pub fn clear_data(
        &mut self,
        tries: ShardTries,
        gc_blocks_limit: NumBlocks,
    ) -> Result<(), Error> {
        let _d = DelayDetector::new(|| "GC".into());

        let head = self.store.head()?;
        let tail = self.store.tail()?;
        let gc_stop_height = self.runtime_adapter.get_gc_stop_height(&head.last_block_hash);

        if gc_stop_height > head.height {
            return Err(ErrorKind::GCError(
                "gc_stop_height cannot be larger than head.height".into(),
            )
            .into());
        }
        let prev_epoch_id = self.get_block_header(&head.prev_block_hash)?.epoch_id();
        let epoch_change = prev_epoch_id != &head.epoch_id;
        let mut fork_tail = self.store.fork_tail()?;
        if epoch_change && fork_tail < gc_stop_height {
            // if head doesn't change on the epoch boundary, we may update fork tail several times
            // but that is fine since it doesn't affect correctness and also we limit the number of
            // heights that fork cleaning goes through so it doesn't slow down client either.
            let mut chain_store_update = self.store.store_update();
            chain_store_update.update_fork_tail(gc_stop_height);
            chain_store_update.commit()?;
            fork_tail = gc_stop_height;
        }
        let mut gc_blocks_remaining = gc_blocks_limit;

        // Forks Cleaning
        let stop_height = std::cmp::max(tail, fork_tail.saturating_sub(GC_FORK_CLEAN_STEP));
        for height in (stop_height..fork_tail).rev() {
            self.clear_forks_data(tries.clone(), height, &mut gc_blocks_remaining)?;
            if gc_blocks_remaining == 0 {
                return Ok(());
            }
            let mut chain_store_update = self.store.store_update();
            chain_store_update.update_fork_tail(height);
            chain_store_update.commit()?;
        }

        // Canonical Chain Clearing
        for height in tail + 1..gc_stop_height {
            if gc_blocks_remaining == 0 {
                return Ok(());
            }
            let mut chain_store_update = self.store.store_update();
            if let Ok(blocks_current_height) =
                chain_store_update.get_chain_store().get_all_block_hashes_by_height(height)
            {
                let blocks_current_height =
                    blocks_current_height.values().flatten().cloned().collect::<Vec<_>>();
                if let Some(block_hash) = blocks_current_height.first() {
                    let prev_hash = *chain_store_update.get_block_header(block_hash)?.prev_hash();
                    let prev_block_refcount = *chain_store_update.get_block_refcount(&prev_hash)?;
                    if prev_block_refcount > 1 {
                        // Block of `prev_hash` starts a Fork, stopping
                        break;
                    } else if prev_block_refcount == 1 {
                        debug_assert_eq!(blocks_current_height.len(), 1);
                        chain_store_update.clear_block_data(
                            &*self.runtime_adapter,
                            *block_hash,
                            GCMode::Canonical(tries.clone()),
                        )?;
                        gc_blocks_remaining -= 1;
                    } else {
                        return Err(ErrorKind::GCError(
                            "block on canonical chain shouldn't have refcount 0".into(),
                        )
                        .into());
                    }
                }
            }
            chain_store_update.update_tail(height);
            chain_store_update.commit()?;
        }
        Ok(())
    }

    /// Garbage collect data which archival node doesn’t need to keep.
    ///
    /// Normally, archival nodes keep all the data from the genesis block and
    /// don’t run garbage collection.  On the other hand, for better performance
    /// the storage contains some data duplication, i.e. values in some of the
    /// columns can be recomputed from data in different columns.  To save on
    /// storage, archival nodes do garbage collect that data.
    ///
    /// `gc_height_limit` limits how many heights will the function process.
    pub fn clear_archive_data(&mut self, gc_height_limit: BlockHeightDelta) -> Result<(), Error> {
        let _d = DelayDetector::new(|| "GC".into());

        let head = self.store.head()?;
        let gc_stop_height = self.runtime_adapter.get_gc_stop_height(&head.last_block_hash);
        if gc_stop_height > head.height {
            return Err(ErrorKind::GCError(
                "gc_stop_height cannot be larger than head.height".into(),
            )
            .into());
        }

        let mut chain_store_update = self.store.store_update();
        chain_store_update.clear_redundant_chunk_data(gc_stop_height, gc_height_limit)?;
        chain_store_update.commit()
    }

    pub fn clear_forks_data(
        &mut self,
        tries: ShardTries,
        height: BlockHeight,
        gc_blocks_remaining: &mut NumBlocks,
    ) -> Result<(), Error> {
        if let Ok(blocks_current_height) = self.store.get_all_block_hashes_by_height(height) {
            let blocks_current_height =
                blocks_current_height.values().flatten().cloned().collect::<Vec<_>>();
            for block_hash in blocks_current_height.iter() {
                let mut current_hash = *block_hash;
                loop {
                    if *gc_blocks_remaining == 0 {
                        return Ok(());
                    }
                    // Block `block_hash` is not on the Canonical Chain
                    // because shorter chain cannot be Canonical one
                    // and it may be safely deleted
                    // and all its ancestors while there are no other sibling blocks rely on it.
                    let mut chain_store_update = self.store.store_update();
                    if *chain_store_update.get_block_refcount(&current_hash)? == 0 {
                        let prev_hash =
                            *chain_store_update.get_block_header(&current_hash)?.prev_hash();

                        // It's safe to call `clear_block_data` for prev data because it clears fork only here
                        chain_store_update.clear_block_data(
                            &*self.runtime_adapter,
                            current_hash,
                            GCMode::Fork(tries.clone()),
                        )?;
                        chain_store_update.commit()?;
                        *gc_blocks_remaining -= 1;

                        current_hash = prev_hash;
                    } else {
                        // Block of `current_hash` is an ancestor for some other blocks, stopping
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Do basic validation of a block upon receiving it. Check that block is
    /// well-formed (various roots match).
    pub fn validate_block(&mut self, block: &MaybeValidated<Block>) -> Result<(), Error> {
        block
            .validate_with(|block| {
                Chain::validate_block_impl(
                    self.runtime_adapter.as_ref(),
                    self.genesis_block(),
                    block,
                )
                .map(|_| true)
            })
            .map(|_| ())
    }

    fn validate_block_impl(
        runtime_adapter: &dyn RuntimeAdapter,
        genesis_block: &Block,
        block: &Block,
    ) -> Result<(), Error> {
        for (shard_id, chunk_header) in block.chunks().iter().enumerate() {
            if chunk_header.height_created() == genesis_block.header().height() {
                // Special case: genesis chunks can be in non-genesis blocks and don't have a signature
                // We must verify that content matches and signature is empty.
                // TODO: this code will not work when genesis block has different number of chunks as the current block
                // https://github.com/near/nearcore/issues/4908
                let genesis_chunk = &genesis_block.chunks()[shard_id];
                if genesis_chunk.chunk_hash() != chunk_header.chunk_hash()
                    || genesis_chunk.signature() != chunk_header.signature()
                {
                    return Err(ErrorKind::InvalidChunk.into());
                }
            } else if chunk_header.height_created() == block.header().height() {
                if !runtime_adapter.verify_chunk_header_signature(
                    &chunk_header.clone(),
                    block.header().epoch_id(),
                    block.header().prev_hash(),
                )? {
                    byzantine_assert!(false);
                    return Err(ErrorKind::InvalidChunk.into());
                }
            }
        }
        block.check_validity().map_err(|e| e.into())
    }

    /// Process a block header received during "header first" propagation.
    pub fn process_block_header(
        &mut self,
        header: &BlockHeader,
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<(), Error> {
        // We create new chain update, but it's not going to be committed so it's read only.
        let mut chain_update = self.chain_update();
        chain_update.process_block_header(header, on_challenge)?;
        Ok(())
    }

    pub fn mark_block_as_challenged(
        &mut self,
        block_hash: &CryptoHash,
        challenger_hash: &CryptoHash,
    ) -> Result<(), Error> {
        let mut chain_update = self.chain_update();
        chain_update.mark_block_as_challenged(block_hash, Some(challenger_hash))?;
        chain_update.commit()?;
        Ok(())
    }

    /// Process a received or produced block, and unroll any orphans that may depend on it.
    /// Changes current state, and calls `block_accepted` callback in case block was successfully applied.
    pub fn process_block(
        &mut self,
        me: &Option<AccountId>,
        block: MaybeValidated<Block>,
        provenance: Provenance,
        block_accepted: &mut dyn FnMut(AcceptedBlock),
        block_misses_chunks: &mut dyn FnMut(BlockMissingChunks),
        block_orphaned_with_missing_chunks: &mut dyn FnMut(OrphanMissingChunks),
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<Option<Tip>, Error> {
        self.blocks_delay_tracker.mark_block_received(block.get_inner(), Clock::instant());
        let block_hash = *block.hash();
        let res = self.process_block_single(
            me,
            block,
            provenance,
            block_accepted,
            block_misses_chunks,
            block_orphaned_with_missing_chunks,
            on_challenge,
        );
        if res.is_ok() {
            if let Some(new_res) = self.check_orphans(
                me,
                block_hash,
                block_accepted,
                block_misses_chunks,
                block_orphaned_with_missing_chunks,
                on_challenge,
            ) {
                return Ok(Some(new_res));
            }
        }
        res
    }

    /// Process challenge to invalidate chain. This is done between blocks to unroll the chain as
    /// soon as possible and allow next block producer to skip invalid blocks.
    pub fn process_challenge(&mut self, challenge: &Challenge) {
        let head = unwrap_or_return!(self.head());
        let mut chain_update = self.chain_update();
        match chain_update.verify_challenges(
            &vec![challenge.clone()],
            &head.epoch_id,
            &head.last_block_hash,
            None,
        ) {
            Ok(_) => {}
            Err(err) => {
                warn!(target: "chain", "Invalid challenge: {}\nChallenge: {:#?}", err, challenge);
            }
        }
        unwrap_or_return!(chain_update.commit());
    }

    /// Processes headers and adds them to store for syncing.
    pub fn sync_block_headers(
        &mut self,
        mut headers: Vec<BlockHeader>,
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<(), Error> {
        // Sort headers by heights if they are out of order.
        headers.sort_by_key(|left| left.height());

        if let Some(header) = headers.first() {
            debug!(target: "chain", "Sync block headers: {} headers from {} at {}", headers.len(), header.hash(), header.height());
        } else {
            return Ok(());
        };

        let all_known = if let Some(last_header) = headers.last() {
            self.store.get_block_header(last_header.hash()).is_ok()
        } else {
            false
        };

        if !all_known {
            // Validate header and then add to the chain.
            for header in headers.iter() {
                let mut chain_update = self.chain_update();

                match check_header_known(&chain_update, header)? {
                    Ok(_) => {}
                    Err(_) => continue,
                }

                chain_update.validate_header(header, &Provenance::SYNC, on_challenge)?;
                chain_update.chain_store_update.save_block_header(header.clone())?;

                // Add validator proposals for given header.
                let last_finalized_height =
                    chain_update.chain_store_update.get_block_height(header.last_final_block())?;
                let epoch_manager_update = chain_update
                    .runtime_adapter
                    .add_validator_proposals(BlockHeaderInfo::new(header, last_finalized_height))?;
                chain_update.chain_store_update.merge(epoch_manager_update);
                chain_update.commit()?;
            }
        }

        let mut chain_update = self.chain_update();

        if let Some(header) = headers.last() {
            // Update header_head if it's the new tip
            chain_update.update_header_head_if_not_challenged(header)?;
        }

        chain_update.commit()
    }

    /// Returns if given block header is on the current chain.
    pub fn is_on_current_chain(&mut self, header: &BlockHeader) -> Result<(), Error> {
        let chain_header = self.get_header_by_height(header.height())?;
        if chain_header.hash() == header.hash() {
            Ok(())
        } else {
            Err(ErrorKind::Other(format!("{} not on current chain", header.hash())).into())
        }
    }

    /// Finds first of the given hashes that is known on the main chain.
    pub fn find_common_header(&mut self, hashes: &[CryptoHash]) -> Option<BlockHeader> {
        for hash in hashes {
            if let Ok(header) = self.get_block_header(hash).map(|h| h.clone()) {
                if let Ok(header_at_height) = self.get_header_by_height(header.height()) {
                    if header.hash() == header_at_height.hash() {
                        return Some(header);
                    }
                }
            }
        }
        None
    }

    fn determine_status(&self, head: Option<Tip>, prev_head: Tip) -> BlockStatus {
        let has_head = head.is_some();
        let mut is_next_block = false;

        let old_hash = if let Some(head) = head {
            if head.prev_block_hash == prev_head.last_block_hash {
                is_next_block = true;
                None
            } else {
                Some(prev_head.last_block_hash)
            }
        } else {
            None
        };

        match (has_head, is_next_block) {
            (true, true) => BlockStatus::Next,
            (true, false) => BlockStatus::Reorg(old_hash.unwrap()),
            (false, _) => BlockStatus::Fork,
        }
    }

    pub fn reset_data_pre_state_sync(&mut self, sync_hash: CryptoHash) -> Result<(), Error> {
        let head = self.head()?;
        // Get header we were syncing into.
        let header = self.get_block_header(&sync_hash)?;
        let prev_hash = *header.prev_hash();
        let sync_height = header.height();
        let gc_height = std::cmp::min(head.height + 1, sync_height);

        // GC all the data from current tail up to `gc_height`. In case tail points to a height where
        // there is no block, we need to make sure that the last block before tail is cleaned.
        let tail = self.store.tail()?;
        let mut tail_prev_block_cleaned = false;
        for height in tail..gc_height {
            if let Ok(blocks_current_height) = self.store.get_all_block_hashes_by_height(height) {
                let blocks_current_height =
                    blocks_current_height.values().flatten().cloned().collect::<Vec<_>>();
                for block_hash in blocks_current_height {
                    let runtime_adapter = self.runtime_adapter();
                    let mut chain_store_update = self.mut_store().store_update();
                    if !tail_prev_block_cleaned {
                        let prev_block_hash =
                            *chain_store_update.get_block_header(&block_hash)?.prev_hash();
                        if chain_store_update.get_block(&prev_block_hash).is_ok() {
                            chain_store_update.clear_block_data(
                                &*runtime_adapter,
                                prev_block_hash,
                                GCMode::StateSync { clear_block_info: true },
                            )?;
                        }
                        tail_prev_block_cleaned = true;
                    }
                    chain_store_update.clear_block_data(
                        &*runtime_adapter,
                        block_hash,
                        GCMode::StateSync { clear_block_info: block_hash != prev_hash },
                    )?;
                    chain_store_update.commit()?;
                }
            }
        }

        // Clear Chunks data
        let mut chain_store_update = self.mut_store().store_update();
        // The largest height of chunk we have in storage is head.height + 1
        let chunk_height = std::cmp::min(head.height + 2, sync_height);
        chain_store_update.clear_chunk_data_and_headers(chunk_height)?;
        chain_store_update.commit()?;

        // clear all trie data

        let tries = self.runtime_adapter.get_tries();
        let mut chain_store_update = self.mut_store().store_update();
        let mut store_update = StoreUpdate::new_with_tries(tries);
        store_update.delete_all(ColState);
        chain_store_update.merge(store_update);

        // The reason to reset tail here is not to allow Tail be greater than Head
        chain_store_update.reset_tail();
        chain_store_update.commit()?;
        Ok(())
    }

    /// Set the new head after state sync was completed if it is indeed newer.
    /// Check for potentially unlocked orphans after this update.
    pub fn reset_heads_post_state_sync(
        &mut self,
        me: &Option<AccountId>,
        sync_hash: CryptoHash,
        block_accepted: &mut dyn FnMut(AcceptedBlock),
        block_misses_chunks: &mut dyn FnMut(BlockMissingChunks),
        orphan_misses_chunks: &mut dyn FnMut(OrphanMissingChunks),
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<(), Error> {
        // Get header we were syncing into.
        let header = self.get_block_header(&sync_hash)?;
        let hash = *header.prev_hash();
        let prev_block = self.get_block(&hash)?;
        let new_tail = prev_block.header().height();
        let new_chunk_tail = prev_block.chunks().iter().map(|x| x.height_created()).min().unwrap();
        let tip = Tip::from_header(prev_block.header());
        let final_head = Tip::from_header(self.genesis.header());
        // Update related heads now.
        let mut chain_store_update = self.mut_store().store_update();
        chain_store_update.save_body_head(&tip)?;
        // Reset final head to genesis since at this point we don't have the last final block.
        chain_store_update.save_final_head(&final_head)?;
        // New Tail can not be earlier than `prev_block.header.inner_lite.height`
        chain_store_update.update_tail(new_tail);
        // New Chunk Tail can not be earlier than minimum of height_created in Block `prev_block`
        chain_store_update.update_chunk_tail(new_chunk_tail);
        chain_store_update.commit()?;

        // Check if there are any orphans unlocked by this state sync.
        // We can't fail beyond this point because the caller will not process accepted blocks
        //    and the blocks with missing chunks if this method fails
        self.check_orphans(
            me,
            hash,
            block_accepted,
            block_misses_chunks,
            orphan_misses_chunks,
            on_challenge,
        );
        Ok(())
    }

    // Processes a single block, increments the metric for the number of blocks processing and also
    // for the number of blocks processed successfully (returns OK).
    fn process_block_single(
        &mut self,
        me: &Option<AccountId>,
        block: MaybeValidated<Block>,
        provenance: Provenance,
        block_accepted: &mut dyn FnMut(AcceptedBlock),
        block_misses_chunks: &mut dyn FnMut(BlockMissingChunks),
        orphan_misses_chunks: &mut dyn FnMut(OrphanMissingChunks),
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<Option<Tip>, Error> {
        metrics::BLOCK_PROCESSING_ATTEMPTS_TOTAL.inc();
        metrics::NUM_ORPHANS.set(self.orphans.len() as i64);
        let block_hash = *block.hash();
        let _timer = CryptoHashTimer::new(block_hash);
        let success_timer = metrics::BLOCK_PROCESSING_TIME.start_timer();

        let res = self.process_block_single_impl(
            me,
            block,
            provenance,
            block_accepted,
            block_misses_chunks,
            orphan_misses_chunks,
            on_challenge,
        );

        if res.is_ok() {
            metrics::BLOCK_PROCESSED_TOTAL.inc();
            success_timer.stop_and_record();
        } else {
            success_timer.stop_and_discard();
        }
        res
    }

    // Block processing. Unlike process_block_single() this function doesn't update metrics for
    // successful blocks processing.
    fn process_block_single_impl(
        &mut self,
        me: &Option<AccountId>,
        block: MaybeValidated<Block>,
        provenance: Provenance,
        block_accepted: &mut dyn FnMut(AcceptedBlock),
        block_misses_chunks: &mut dyn FnMut(BlockMissingChunks),
        orphan_misses_chunks: &mut dyn FnMut(OrphanMissingChunks),
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<Option<Tip>, Error> {
        let prev_head = self.store.head()?;
        let mut chain_update = self.chain_update();
        let maybe_new_head = chain_update.process_block(me, &block, &provenance, on_challenge);
        let block_height = block.header().height();

        match maybe_new_head {
            Ok(head) => {
                chain_update.chain_store_update.save_block_height_processed(block_height);
                chain_update.commit()?;

                self.pending_states_to_patch = None;

                match &head {
                    Some(tip) => {
                        if let Ok(producers) = self
                            .runtime_adapter
                            .get_epoch_block_producers_ordered(&tip.epoch_id, &tip.last_block_hash)
                        {
                            let mut count = 0;
                            let mut stake = 0;
                            for (info, is_slashed) in producers.iter() {
                                if !*is_slashed {
                                    stake += info.stake();
                                    count += 1;
                                }
                            }
                            stake /= NEAR_BASE;
                            metrics::VALIDATOR_AMOUNT_STAKED
                                .set(i64::try_from(stake).unwrap_or(i64::MAX));
                            metrics::VALIDATOR_ACTIVE_TOTAL.set(count);
                        }
                    }
                    None => {}
                }

                let status = self.determine_status(head.clone(), prev_head);

                // Notify other parts of the system of the update.
                block_accepted(AcceptedBlock { hash: *block.hash(), status, provenance });

                Ok(head)
            }
            Err(e) => {
                match e.kind() {
                    ErrorKind::Orphan => {
                        let tail_height = self.store.tail()?;
                        // we only add blocks that couldn't have been gc'ed to the orphan pool.
                        if block_height >= tail_height {
                            let block_hash = *block.hash();
                            let requested_missing_chunks = if let Some(orphan_missing_chunks) =
                                self.should_request_chunks_for_orphan(me, &block)
                            {
                                debug!(target:"chain", "Request missing chunks for orphan {:?} {:?}", block_hash, orphan_missing_chunks.missing_chunks);
                                // This callback handles requesting missing chunks. It adds the missing chunks
                                // to a list and all missing chunks in the list will be requested
                                // at the end of Client::process_block
                                orphan_misses_chunks(orphan_missing_chunks);
                                true
                            } else {
                                false
                            };

                            let time = Clock::instant();
                            self.blocks_delay_tracker.mark_block_orphaned(block.hash(), time);
                            let orphan = Orphan { block, provenance, added: time };
                            self.orphans.add(orphan, requested_missing_chunks);

                            debug!(
                                target: "chain",
                                "Process block: orphan: {:?}, # orphans {}{}",
                                block_hash,
                                self.orphans.len(),
                                if self.orphans.len_evicted() > 0 {
                                    format!(", # evicted {}", self.orphans.len_evicted())
                                } else {
                                    String::new()
                                },
                            );
                        }
                    }
                    ErrorKind::ChunksMissing(missing_chunks) => {
                        let block_hash = *block.hash();
                        let missing_chunk_hashes: Vec<_> =
                            missing_chunks.iter().map(|header| header.chunk_hash()).collect();
                        block_misses_chunks(BlockMissingChunks {
                            prev_hash: *block.header().prev_hash(),
                            missing_chunks,
                            block_hash,
                        });
                        let time = Clock::instant();
                        self.blocks_delay_tracker.mark_block_has_missing_chunks(block.hash(), time);
                        let orphan = Orphan { block, provenance, added: time };
                        self.blocks_with_missing_chunks
                            .add_block_with_missing_chunks(orphan, missing_chunk_hashes.clone());
                        debug!(
                            target: "chain",
                            "Process block: missing chunks. Block hash: {:?}. Missing chunks: {:?}",
                            block_hash, missing_chunk_hashes,
                        );
                    }
                    ErrorKind::EpochOutOfBounds(ref epoch_id) => {
                        // Possibly block arrived before we finished processing all of the blocks for epoch before last.
                        // Or someone is attacking with invalid chain.
                        debug!(target: "chain", "Received block {}/{} ignored, as epoch {:?} is unknown", block_height, block.hash(), epoch_id);
                    }
                    ErrorKind::BlockKnown(ref block_known_error) => {
                        debug!(
                            target: "chain",
                            "Block {} at {} is known at this time: {:?}",
                            block.hash(),
                            block_height,
                            block_known_error);
                    }
                    _ => {}
                }
                if let Err(e) = self.save_block_height_processed(block_height) {
                    warn!(target: "chain", "Failed to save processed height {}: {}", block_height, e);
                }
                Err(e)
            }
        }
    }

    /// Check if we can request chunks for this orphan. Conditions are
    /// 1) Orphans that with outstanding missing chunks request has not exceed `MAX_ORPHAN_MISSING_CHUNKS`
    /// 2) we haven't already requested missing chunks for the orphan
    /// 3) All the `NUM_ORPHAN_ANCESTORS_CHECK` immediate parents of the block are either accepted,
    ///    or orphans or in `blocks_with_missing_chunks`
    /// 4) Among the `NUM_ORPHAN_ANCESTORS_CHECK` immediate parents of the block at least one is
    ///    accepted(is in store), call it `ancestor`
    /// 5) The next block of `ancestor` has the same epoch_id as the orphan block
    ///    (This is because when requesting chunks, we will use `ancestor` hash instead of the
    ///     previous block hash of the orphan to decide epoch id)
    /// 6) The orphan has missing chunks
    pub fn should_request_chunks_for_orphan(
        &mut self,
        me: &Option<AccountId>,
        orphan: &Block,
    ) -> Option<OrphanMissingChunks> {
        // 1) Orphans that with outstanding missing chunks request has not exceed `MAX_ORPHAN_MISSING_CHUNKS`
        // 2) we haven't already requested missing chunks for the orphan
        if !self.orphans.can_request_missing_chunks_for_orphan(orphan.hash()) {
            return None;
        }
        let mut block_hash = *orphan.header().prev_hash();
        for _ in 0..NUM_ORPHAN_ANCESTORS_CHECK {
            // 3) All the `NUM_ORPHAN_ANCESTORS_CHECK` immediate parents of the block are either accepted,
            //    or orphans or in `blocks_with_missing_chunks`
            if let Some(block) = self.blocks_with_missing_chunks.get(&block_hash) {
                block_hash = *block.prev_hash();
                continue;
            }
            if let Some(orphan) = self.orphans.get(&block_hash) {
                block_hash = *orphan.prev_hash();
                continue;
            }
            // 4) Among the `NUM_ORPHAN_ANCESTORS_CHECK` immediate parents of the block at least one is
            //    accepted(is in store), call it `ancestor`
            if self.get_block(&block_hash).is_ok() {
                if let Ok(epoch_id) = self.runtime_adapter.get_epoch_id_from_prev_block(&block_hash)
                {
                    // 5) The next block of `ancestor` has the same epoch_id as the orphan block
                    if &epoch_id == orphan.header().epoch_id() {
                        let mut chain_update = self.chain_update();
                        // 6) The orphan has missing chunks
                        if let Err(e) = chain_update.ping_missing_chunks(me, block_hash, orphan) {
                            return match e.kind() {
                                ErrorKind::ChunksMissing(missing_chunks) => {
                                    Some(OrphanMissingChunks {
                                        missing_chunks,
                                        epoch_id,
                                        ancestor_hash: block_hash,
                                        requestor_block_hash: *orphan.header().hash(),
                                    })
                                }
                                _ => None,
                            };
                        }
                    }
                }
                return None;
            }
            return None;
        }
        None
    }

    /// only used for test
    pub fn check_orphan_partial_chunks_requested(&self, block_hash: &CryptoHash) -> bool {
        self.orphans.orphans_requested_missing_chunks.contains(block_hash)
    }

    pub fn prev_block_is_caught_up(
        &self,
        prev_prev_hash: &CryptoHash,
        prev_hash: &CryptoHash,
    ) -> Result<bool, Error> {
        // This method is identical to `ChainUpdate::prev_block_is_caught_up`, see important
        //    disclaimers in there on some dangers of using it
        Ok(!self.store.get_blocks_to_catchup(prev_prev_hash)?.contains(prev_hash))
    }

    /// Return all shards that whose states need to be caught up
    /// That has two cases:
    /// 1) Shard layout will change in the next epoch. In this case, the method returns all shards
    ///    in the current epoch that will be split into a future shard that `me` will track.
    /// 2) Shard layout will be the same. In this case, the method returns all shards that `me` will
    ///    track in the next epoch but not this epoch
    fn get_shards_to_dl_state(
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        me: &Option<AccountId>,
        parent_hash: &CryptoHash,
    ) -> Result<Vec<ShardId>, Error> {
        let epoch_id = runtime_adapter.get_epoch_id_from_prev_block(parent_hash)?;
        Ok((0..runtime_adapter.num_shards(&epoch_id)?)
            .filter(|shard_id| {
                Self::should_catch_up_shard(runtime_adapter.clone(), me, parent_hash, *shard_id)
            })
            .collect())
    }

    fn should_catch_up_shard(
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        me: &Option<AccountId>,
        parent_hash: &CryptoHash,
        shard_id: ShardId,
    ) -> bool {
        let will_shard_layout_change =
            runtime_adapter.will_shard_layout_change_next_epoch(parent_hash).unwrap_or(false);
        // if shard layout will change the next epoch, we should catch up the shard regardless
        // whether we already have the shard's state this epoch, because we need to generate
        // new states for shards split from the current shard for the next epoch
        runtime_adapter.will_care_about_shard(me.as_ref(), parent_hash, shard_id, true)
            && (will_shard_layout_change
                || !runtime_adapter.cares_about_shard(me.as_ref(), parent_hash, shard_id, true))
    }

    /// Check if any block with missing chunk is ready to be processed
    pub fn check_blocks_with_missing_chunks(
        &mut self,
        me: &Option<AccountId>,
        block_accepted: &mut dyn FnMut(AcceptedBlock),
        block_misses_chunks: &mut dyn FnMut(BlockMissingChunks),
        orphan_misses_chunks: &mut dyn FnMut(OrphanMissingChunks),
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) {
        let mut new_blocks_accepted = vec![];
        let orphans = self.blocks_with_missing_chunks.ready_blocks();
        for orphan in orphans {
            let block_hash = *orphan.block.header().hash();
            let time = Clock::instant();
            let res = self.process_block_single(
                me,
                orphan.block,
                orphan.provenance,
                block_accepted,
                block_misses_chunks,
                orphan_misses_chunks,
                on_challenge,
            );
            match res {
                Ok(_) => {
                    debug!(target: "chain", "Block with missing chunks is accepted; me: {:?}", me);
                    self.blocks_delay_tracker
                        .mark_block_completed_missing_chunks(&block_hash, time);
                    new_blocks_accepted.push(block_hash);
                }
                Err(_) => {
                    debug!(target: "chain", "Block with missing chunks is declined; me: {:?}", me);
                }
            }
        }

        for accepted_block in new_blocks_accepted {
            self.check_orphans(
                me,
                accepted_block,
                block_accepted,
                block_misses_chunks,
                orphan_misses_chunks,
                on_challenge,
            );
        }
    }

    /// Check for orphans that are ready to be processed or request missing chunks, once a block
    /// is successfully accepted.
    /// `prev_hash`: hash of the block that is just accepted
    /// `block_accepted`: callback to be called when an orphan is accepted
    /// `block_misses_chunks`: callback to be called when an orphan is added to the pool of blocks
    ///                        that have missing chunks
    /// `orphan_misses_chunks`: callback to be called when it is ready to request missing chunks for
    ///                         an orphan
    /// `on_challenge`: callback to be called when an orphan should be challenged
    pub fn check_orphans(
        &mut self,
        me: &Option<AccountId>,
        prev_hash: CryptoHash,
        block_accepted: &mut dyn FnMut(AcceptedBlock),
        block_misses_chunks: &mut dyn FnMut(BlockMissingChunks),
        orphan_misses_chunks: &mut dyn FnMut(OrphanMissingChunks),
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Option<Tip> {
        let mut queue = vec![prev_hash];
        let mut queue_idx = 0;

        let mut maybe_new_head = None;

        // Check if there are orphans we can process.
        debug!(target: "chain", "Check orphans: from {}, # orphans {}", prev_hash, self.orphans.len());
        while queue_idx < queue.len() {
            let prev_hash = queue[queue_idx];
            // check within the descendents of `prev_hash` to see if there are orphans there that
            // are ready to request missing chunks for
            let orphans_to_check =
                self.orphans.get_orphans_within_depth(prev_hash, NUM_ORPHAN_ANCESTORS_CHECK);
            for orphan_hash in orphans_to_check {
                let orphan = self.orphans.get(&orphan_hash).unwrap().block.clone();
                if let Some(orphan_missing_chunks) =
                    self.should_request_chunks_for_orphan(me, &orphan)
                {
                    debug!(target:"chain", "Request missing chunks for orphan {:?}", orphan_hash);
                    orphan_misses_chunks(orphan_missing_chunks);
                    self.orphans.mark_missing_chunks_requested_for_orphan(orphan_hash);
                }
            }
            if let Some(orphans) = self.orphans.remove_by_prev_hash(prev_hash) {
                debug!(target: "chain", "Check orphans: found {} orphans", orphans.len());
                for orphan in orphans.into_iter() {
                    let block_hash = orphan.hash();
                    self.blocks_delay_tracker.mark_block_unorphaned(&block_hash, Clock::instant());
                    let res = self.process_block_single(
                        me,
                        orphan.block,
                        orphan.provenance,
                        block_accepted,
                        block_misses_chunks,
                        orphan_misses_chunks,
                        on_challenge,
                    );
                    match res {
                        Ok(maybe_tip) => {
                            maybe_new_head = maybe_tip;
                            queue.push(block_hash);
                        }
                        Err(_) => {
                            debug!(target: "chain", "Orphan declined");
                        }
                    }
                }
            }
            queue_idx += 1;
        }

        if queue.len() > 1 {
            debug!(
                target: "chain",
                "Check orphans: {} blocks accepted, remaining # orphans {}",
                queue.len() - 1,
                self.orphans.len(),
            );
        }

        maybe_new_head
    }

    pub fn get_outgoing_receipts_for_shard(
        &mut self,
        prev_block_hash: CryptoHash,
        shard_id: ShardId,
        last_height_included: BlockHeight,
    ) -> Result<Vec<Receipt>, Error> {
        self.store.get_outgoing_receipts_for_shard(
            &*self.runtime_adapter,
            prev_block_hash,
            shard_id,
            last_height_included,
        )
    }

    pub fn get_state_response_header(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
    ) -> Result<ShardStateSyncResponseHeader, Error> {
        // Check cache
        let key = StateHeaderKey(shard_id, sync_hash).try_to_vec()?;
        if let Ok(Some(header)) = self.store.store().get_ser(ColStateHeaders, &key) {
            return Ok(header);
        }

        // Consistency rules:
        // 1. Everything prefixed with `sync_` indicates new epoch, for which we are syncing.
        // 1a. `sync_prev` means the last of the prev epoch.
        // 2. Empty prefix means the height where chunk was applied last time in the prev epoch.
        //    Let's call it `current`.
        // 2a. `prev_` means we're working with height before current.
        // 3. In inner loops we use all prefixes with no relation to the context described above.
        let sync_block = self
            .get_block(&sync_hash)
            .log_storage_error("block has already been checked for existence")?;
        let sync_block_header = sync_block.header().clone();
        let sync_block_epoch_id = sync_block.header().epoch_id().clone();
        if shard_id as usize >= sync_block.chunks().len() {
            return Err(ErrorKind::InvalidStateRequest("ShardId out of bounds".into()).into());
        }

        // The chunk was applied at height `chunk_header.height_included`.
        // Getting the `current` state.
        let sync_prev_block = self.get_block(sync_block_header.prev_hash())?;
        if &sync_block_epoch_id == sync_prev_block.header().epoch_id() {
            return Err(ErrorKind::InvalidStateRequest(
                "sync_hash is not the first hash of the epoch".into(),
            )
            .into());
        }
        if shard_id as usize >= sync_prev_block.chunks().len() {
            return Err(ErrorKind::InvalidStateRequest("ShardId out of bounds".into()).into());
        }
        // Chunk header here is the same chunk header as at the `current` height.
        let sync_prev_hash = *sync_prev_block.hash();
        let chunk_header = sync_prev_block.chunks()[shard_id as usize].clone();
        let (chunk_headers_root, chunk_proofs) = merklize(
            &sync_prev_block
                .chunks()
                .iter()
                .map(|shard_chunk| {
                    ChunkHashHeight(shard_chunk.chunk_hash(), shard_chunk.height_included())
                })
                .collect::<Vec<ChunkHashHeight>>(),
        );
        assert_eq!(&chunk_headers_root, sync_prev_block.header().chunk_headers_root());

        let chunk = self.get_chunk_clone_from_header(&chunk_header)?;
        let chunk_proof = chunk_proofs[shard_id as usize].clone();
        let block_header =
            self.get_header_on_chain_by_height(&sync_hash, chunk_header.height_included())?.clone();

        // Collecting the `prev` state.
        let (prev_chunk_header, prev_chunk_proof, prev_chunk_height_included) = match self
            .get_block(block_header.prev_hash())
        {
            Ok(prev_block) => {
                if shard_id as usize >= prev_block.chunks().len() {
                    return Err(
                        ErrorKind::InvalidStateRequest("ShardId out of bounds".into()).into()
                    );
                }
                let prev_chunk_header = prev_block.chunks()[shard_id as usize].clone();
                let (prev_chunk_headers_root, prev_chunk_proofs) = merklize(
                    &prev_block
                        .chunks()
                        .iter()
                        .map(|shard_chunk| {
                            ChunkHashHeight(shard_chunk.chunk_hash(), shard_chunk.height_included())
                        })
                        .collect::<Vec<ChunkHashHeight>>(),
                );
                assert_eq!(&prev_chunk_headers_root, prev_block.header().chunk_headers_root());

                let prev_chunk_proof = prev_chunk_proofs[shard_id as usize].clone();
                let prev_chunk_height_included = prev_chunk_header.height_included();

                (Some(prev_chunk_header), Some(prev_chunk_proof), prev_chunk_height_included)
            }
            Err(e) => match e.kind() {
                ErrorKind::DBNotFoundErr(_) => {
                    if block_header.prev_hash() == &CryptoHash::default() {
                        (None, None, 0)
                    } else {
                        return Err(e);
                    }
                }
                _ => return Err(e),
            },
        };

        // Getting all existing incoming_receipts from prev_chunk height to the new epoch.
        let incoming_receipts_proofs = ChainStoreUpdate::new(&mut self.store)
            .get_incoming_receipts_for_shard(shard_id, sync_hash, prev_chunk_height_included)?;

        // Collecting proofs for incoming receipts.
        let mut root_proofs = vec![];
        for receipt_response in incoming_receipts_proofs.iter() {
            let ReceiptProofResponse(block_hash, receipt_proofs) = receipt_response;
            let block_header = self.get_block_header(block_hash)?.clone();
            let block = self.get_block(block_hash)?;
            let (block_receipts_root, block_receipts_proofs) = merklize(
                &block
                    .chunks()
                    .iter()
                    .map(|chunk| chunk.outgoing_receipts_root())
                    .collect::<Vec<CryptoHash>>(),
            );

            let mut root_proofs_cur = vec![];
            assert_eq!(receipt_proofs.len(), block_header.chunks_included() as usize);
            for receipt_proof in receipt_proofs {
                let ReceiptProof(receipts, shard_proof) = receipt_proof;
                let ShardProof { from_shard_id, to_shard_id: _, proof } = shard_proof;
                let receipts_hash = hash(&ReceiptList(shard_id, receipts).try_to_vec()?);
                let from_shard_id = *from_shard_id as usize;

                let root_proof = block.chunks()[from_shard_id].outgoing_receipts_root();
                root_proofs_cur
                    .push(RootProof(root_proof, block_receipts_proofs[from_shard_id].clone()));

                // Make sure we send something reasonable.
                assert_eq!(block_header.chunk_receipts_root(), &block_receipts_root);
                assert!(verify_path(root_proof, proof, &receipts_hash));
                assert!(verify_path(
                    block_receipts_root,
                    &block_receipts_proofs[from_shard_id],
                    &root_proof,
                ));
            }
            root_proofs.push(root_proofs_cur);
        }

        let state_root_node = self.runtime_adapter.get_state_root_node(
            shard_id,
            &sync_prev_hash,
            &chunk_header.prev_state_root(),
        )?;

        let shard_state_header = match chunk {
            ShardChunk::V1(chunk) => {
                let prev_chunk_header =
                    prev_chunk_header.and_then(|prev_header| match prev_header {
                        ShardChunkHeader::V1(header) => Some(header),
                        ShardChunkHeader::V2(_) => None,
                        ShardChunkHeader::V3(_) => None,
                    });
                ShardStateSyncResponseHeader::V1(ShardStateSyncResponseHeaderV1 {
                    chunk,
                    chunk_proof,
                    prev_chunk_header,
                    prev_chunk_proof,
                    incoming_receipts_proofs,
                    root_proofs,
                    state_root_node,
                })
            }

            chunk @ ShardChunk::V2(_) => {
                ShardStateSyncResponseHeader::V2(ShardStateSyncResponseHeaderV2 {
                    chunk,
                    chunk_proof,
                    prev_chunk_header,
                    prev_chunk_proof,
                    incoming_receipts_proofs,
                    root_proofs,
                    state_root_node,
                })
            }
        };

        // Saving the header data
        let mut store_update = self.store.store().store_update();
        store_update.set_ser(ColStateHeaders, &key, &shard_state_header)?;
        store_update.commit()?;

        Ok(shard_state_header)
    }

    pub fn get_state_response_part(
        &mut self,
        shard_id: ShardId,
        part_id: u64,
        sync_hash: CryptoHash,
    ) -> Result<Vec<u8>, Error> {
        // Check cache
        let key = StatePartKey(sync_hash, shard_id, part_id).try_to_vec()?;
        if let Ok(Some(state_part)) = self.store.store().get(ColStateParts, &key) {
            return Ok(state_part);
        }

        let sync_block = self
            .get_block(&sync_hash)
            .log_storage_error("block has already been checked for existence")?;
        let sync_block_header = sync_block.header().clone();
        let sync_block_epoch_id = sync_block.header().epoch_id().clone();
        if shard_id as usize >= sync_block.chunks().len() {
            return Err(ErrorKind::InvalidStateRequest("shard_id out of bounds".into()).into());
        }
        let sync_prev_block = self.get_block(sync_block_header.prev_hash())?;
        if &sync_block_epoch_id == sync_prev_block.header().epoch_id() {
            return Err(ErrorKind::InvalidStateRequest(
                "sync_hash is not the first hash of the epoch".into(),
            )
            .into());
        }
        if shard_id as usize >= sync_prev_block.chunks().len() {
            return Err(ErrorKind::InvalidStateRequest("shard_id out of bounds".into()).into());
        }
        let state_root = sync_prev_block.chunks()[shard_id as usize].prev_state_root();
        let sync_prev_hash = *sync_prev_block.hash();
        let state_root_node = self
            .runtime_adapter
            .get_state_root_node(shard_id, &sync_prev_hash, &state_root)
            .log_storage_error("get_state_root_node fail")?;
        let num_parts = get_num_state_parts(state_root_node.memory_usage);

        if part_id >= num_parts {
            return Err(ErrorKind::InvalidStateRequest("part_id out of bound".to_string()).into());
        }
        let state_part = self
            .runtime_adapter
            .obtain_state_part(
                shard_id,
                &sync_prev_hash,
                &state_root,
                PartId::new(part_id, num_parts),
            )
            .log_storage_error("obtain_state_part fail")?;

        // Before saving State Part data, we need to make sure we can calculate and save State Header
        self.get_state_response_header(shard_id, sync_hash)?;

        // Saving the part data
        let mut store_update = self.store.store().store_update();
        store_update.set(ColStateParts, &key, &state_part);
        store_update.commit()?;

        Ok(state_part)
    }

    pub fn set_state_header(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        shard_state_header: ShardStateSyncResponseHeader,
    ) -> Result<(), Error> {
        let sync_block_header = self.get_block_header(&sync_hash)?.clone();

        let chunk = shard_state_header.cloned_chunk();
        let prev_chunk_header = shard_state_header.cloned_prev_chunk_header();

        // 1-2. Checking chunk validity
        if !validate_chunk_proofs(&chunk, &*self.runtime_adapter)? {
            byzantine_assert!(false);
            return Err(ErrorKind::Other(
                "set_shard_state failed: chunk header proofs are invalid".into(),
            )
            .into());
        }

        // Consider chunk itself is valid.

        // 3. Checking that chunks `chunk` and `prev_chunk` are included in appropriate blocks
        // 3a. Checking that chunk `chunk` is included into block at last height before sync_hash
        // 3aa. Also checking chunk.height_included
        let sync_prev_block_header = self.get_block_header(sync_block_header.prev_hash())?.clone();
        if !verify_path(
            *sync_prev_block_header.chunk_headers_root(),
            shard_state_header.chunk_proof(),
            &ChunkHashHeight(chunk.chunk_hash(), chunk.height_included()),
        ) {
            byzantine_assert!(false);
            return Err(ErrorKind::Other(
                "set_shard_state failed: chunk isn't included into block".into(),
            )
            .into());
        }

        let block_header =
            self.get_header_on_chain_by_height(&sync_hash, chunk.height_included())?.clone();
        // 3b. Checking that chunk `prev_chunk` is included into block at height before chunk.height_included
        // 3ba. Also checking prev_chunk.height_included - it's important for getting correct incoming receipts
        match (&prev_chunk_header, shard_state_header.prev_chunk_proof()) {
            (Some(prev_chunk_header), Some(prev_chunk_proof)) => {
                let prev_block_header =
                    self.get_block_header(block_header.prev_hash())?.clone();
                if !verify_path(
                    *prev_block_header.chunk_headers_root(),
                    prev_chunk_proof,
                    &ChunkHashHeight(prev_chunk_header.chunk_hash(), prev_chunk_header.height_included()),
                ) {
                    byzantine_assert!(false);
                    return Err(ErrorKind::Other(
                        "set_shard_state failed: prev_chunk isn't included into block".into(),
                    )
                    .into());
                }
            }
            (None, None) => {
                if chunk.height_included() != 0 {
                    return Err(ErrorKind::Other(
                    "set_shard_state failed: received empty state response for a chunk that is not at height 0".into()
                ).into());
                }
            }
            _ =>
                return Err(ErrorKind::Other("set_shard_state failed: `prev_chunk_header` and `prev_chunk_proof` must either both be present or both absent".into()).into())
        };

        // 4. Proving incoming receipts validity
        // 4a. Checking len of proofs
        if shard_state_header.root_proofs().len()
            != shard_state_header.incoming_receipts_proofs().len()
        {
            byzantine_assert!(false);
            return Err(ErrorKind::Other("set_shard_state failed: invalid proofs".into()).into());
        }
        let mut hash_to_compare = sync_hash;
        for (i, receipt_response) in
            shard_state_header.incoming_receipts_proofs().iter().enumerate()
        {
            let ReceiptProofResponse(block_hash, receipt_proofs) = receipt_response;

            // 4b. Checking that there is a valid sequence of continuous blocks
            if *block_hash != hash_to_compare {
                byzantine_assert!(false);
                return Err(ErrorKind::Other(
                    "set_shard_state failed: invalid incoming receipts".into(),
                )
                .into());
            }
            let header = self.get_block_header(&hash_to_compare)?;
            hash_to_compare = *header.prev_hash();

            let block_header = self.get_block_header(block_hash)?;
            // 4c. Checking len of receipt_proofs for current block
            if receipt_proofs.len() != shard_state_header.root_proofs()[i].len()
                || receipt_proofs.len() != block_header.chunks_included() as usize
            {
                byzantine_assert!(false);
                return Err(
                    ErrorKind::Other("set_shard_state failed: invalid proofs".into()).into()
                );
            }
            // We know there were exactly `block_header.chunks_included` chunks included
            // on the height of block `block_hash`.
            // There were no other proofs except for included chunks.
            // According to Pigeonhole principle, it's enough to ensure all receipt_proofs are distinct
            // to prove that all receipts were received and no receipts were hidden.
            let mut visited_shard_ids = HashSet::<ShardId>::new();
            for (j, receipt_proof) in receipt_proofs.iter().enumerate() {
                let ReceiptProof(receipts, shard_proof) = receipt_proof;
                let ShardProof { from_shard_id, to_shard_id: _, proof } = shard_proof;
                // 4d. Checking uniqueness for set of `from_shard_id`
                match visited_shard_ids.get(from_shard_id) {
                    Some(_) => {
                        byzantine_assert!(false);
                        return Err(ErrorKind::Other(
                            "set_shard_state failed: invalid proofs".into(),
                        )
                        .into());
                    }
                    _ => visited_shard_ids.insert(*from_shard_id),
                };
                let RootProof(root, block_proof) = &shard_state_header.root_proofs()[i][j];
                let receipts_hash = hash(&ReceiptList(shard_id, receipts).try_to_vec()?);
                // 4e. Proving the set of receipts is the subset of outgoing_receipts of shard `shard_id`
                if !verify_path(*root, proof, &receipts_hash) {
                    byzantine_assert!(false);
                    return Err(
                        ErrorKind::Other("set_shard_state failed: invalid proofs".into()).into()
                    );
                }
                // 4f. Proving the outgoing_receipts_root matches that in the block
                if !verify_path(*block_header.chunk_receipts_root(), block_proof, root) {
                    byzantine_assert!(false);
                    return Err(
                        ErrorKind::Other("set_shard_state failed: invalid proofs".into()).into()
                    );
                }
            }
        }
        // 4g. Checking that there are no more heights to get incoming_receipts
        let header = self.get_block_header(&hash_to_compare)?;
        if header.height() != prev_chunk_header.map_or(0, |h| h.height_included()) {
            byzantine_assert!(false);
            return Err(ErrorKind::Other(
                "set_shard_state failed: invalid incoming receipts".into(),
            )
            .into());
        }

        // 5. Checking that state_root_node is valid
        let chunk_inner = chunk.take_header().take_inner();
        if !self.runtime_adapter.validate_state_root_node(
            shard_state_header.state_root_node(),
            chunk_inner.prev_state_root(),
        ) {
            byzantine_assert!(false);
            return Err(ErrorKind::Other(
                "set_shard_state failed: state_root_node is invalid".into(),
            )
            .into());
        }

        // Saving the header data.
        let mut store_update = self.store.store().store_update();
        let key = StateHeaderKey(shard_id, sync_hash).try_to_vec()?;
        store_update.set_ser(ColStateHeaders, &key, &shard_state_header)?;
        store_update.commit()?;

        Ok(())
    }

    pub fn get_state_header(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
    ) -> Result<ShardStateSyncResponseHeader, Error> {
        self.store.get_state_header(shard_id, sync_hash)
    }

    pub fn set_state_part(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        part_id: PartId,
        data: &Vec<u8>,
    ) -> Result<(), Error> {
        let shard_state_header = self.get_state_header(shard_id, sync_hash)?;
        let chunk = shard_state_header.take_chunk();
        let state_root = *chunk.take_header().take_inner().prev_state_root();
        if !self.runtime_adapter.validate_state_part(&state_root, part_id, data) {
            byzantine_assert!(false);
            return Err(ErrorKind::Other(
                "set_state_part failed: validate_state_part failed".into(),
            )
            .into());
        }

        // Saving the part data.
        let mut store_update = self.store.store().store_update();
        let key = StatePartKey(sync_hash, shard_id, part_id.idx).try_to_vec()?;
        store_update.set(ColStateParts, &key, data);
        store_update.commit()?;
        Ok(())
    }

    pub fn schedule_apply_state_parts(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        num_parts: u64,
        state_parts_task_scheduler: &dyn Fn(ApplyStatePartsRequest),
    ) -> Result<(), Error> {
        let shard_state_header = self.get_state_header(shard_id, sync_hash)?;
        let state_root = shard_state_header.chunk_prev_state_root();
        let epoch_id = self.get_block_header(&sync_hash)?.epoch_id().clone();

        state_parts_task_scheduler(ApplyStatePartsRequest {
            runtime: Arc::clone(&self.runtime_adapter),
            shard_id,
            state_root,
            num_parts,
            epoch_id,
            sync_hash,
        });

        Ok(())
    }

    pub fn set_state_finalize(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        apply_result: Result<(), near_chain_primitives::Error>,
    ) -> Result<(), Error> {
        apply_result?;

        let shard_state_header = self.get_state_header(shard_id, sync_hash)?;
        let mut height = shard_state_header.chunk_height_included();
        let mut chain_update = self.chain_update();
        chain_update.set_state_finalize(shard_id, sync_hash, shard_state_header)?;
        chain_update.commit()?;

        // We restored the state on height `shard_state_header.chunk.header.height_included`.
        // Now we should build a chain up to height of `sync_hash` block.
        loop {
            height += 1;
            let mut chain_update = self.chain_update();
            // Result of successful execution of set_state_finalize_on_height is bool,
            // should we commit and continue or stop.
            if chain_update.set_state_finalize_on_height(height, shard_id, sync_hash)? {
                chain_update.commit()?;
            } else {
                break;
            }
        }

        Ok(())
    }

    pub fn build_state_for_split_shards_preprocessing(
        &mut self,
        sync_hash: &CryptoHash,
        shard_id: ShardId,
        state_split_scheduler: &dyn Fn(StateSplitRequest),
    ) -> Result<(), Error> {
        let (epoch_id, next_epoch_id) = {
            let block_header = self.get_block_header(sync_hash)?;
            (block_header.epoch_id().clone(), block_header.next_epoch_id().clone())
        };
        let shard_layout = self.runtime_adapter.get_shard_layout(&epoch_id)?;
        let next_epoch_shard_layout = self.runtime_adapter.get_shard_layout(&next_epoch_id)?;
        let shard_uid = ShardUId::from_shard_id_and_layout(shard_id, &shard_layout);
        let prev_hash = *self.get_block_header(sync_hash)?.prev_hash();
        let state_root = *self.get_chunk_extra(&prev_hash, &shard_uid)?.state_root();
        assert_ne!(shard_layout, next_epoch_shard_layout);

        state_split_scheduler(StateSplitRequest {
            runtime: Arc::clone(&self.runtime_adapter),
            sync_hash: *sync_hash,
            shard_id,
            shard_uid,
            state_root: state_root,
            next_epoch_shard_layout,
        });

        Ok(())
    }

    pub fn build_state_for_split_shards_postprocessing(
        &mut self,
        sync_hash: &CryptoHash,
        state_roots: Result<HashMap<ShardUId, StateRoot>, Error>,
    ) -> Result<(), Error> {
        let prev_hash = *self.get_block_header(sync_hash)?.prev_hash();
        let mut chain_update = self.chain_update();
        for (shard_uid, state_root) in state_roots? {
            // here we store the state roots in chunk_extra in the database for later use
            let chunk_extra = ChunkExtra::new_with_only_state_root(&state_root);
            chain_update.chain_store_update.save_chunk_extra(&prev_hash, &shard_uid, chunk_extra);
            debug!(target:"chain", "Finish building split state for shard {:?} {:?} {:?} ", shard_uid, prev_hash, state_root);
        }
        chain_update.commit()
    }

    pub fn clear_downloaded_parts(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        num_parts: u64,
    ) -> Result<(), Error> {
        let mut chain_store_update = self.mut_store().store_update();
        chain_store_update.gc_col_state_parts(sync_hash, shard_id, num_parts)?;
        Ok(chain_store_update.commit()?)
    }

    pub fn catchup_blocks_step(
        &mut self,
        me: &Option<AccountId>,
        sync_hash: &CryptoHash,
        blocks_catch_up_state: &mut BlocksCatchUpState,
        block_catch_up_scheduler: &dyn Fn(BlockCatchUpRequest),
    ) -> Result<(), Error> {
        debug!(target:"catchup", "catch up blocks: pending blocks: {:?}, processed {:?}, scheduled: {:?}, done: {:?}",
               blocks_catch_up_state.pending_blocks, blocks_catch_up_state.processed_blocks.keys().collect::<Vec<_>>(),
               blocks_catch_up_state.scheduled_blocks.keys().collect::<Vec<_>>(), blocks_catch_up_state.done_blocks.len());
        for (queued_block, (saved_store_update, results)) in
            blocks_catch_up_state.processed_blocks.drain()
        {
            match self.block_catch_up_postprocess(&queued_block, results, saved_store_update) {
                Ok(_) => {
                    let mut saw_one = false;
                    for next_block_hash in self.store.get_blocks_to_catchup(&queued_block)?.clone()
                    {
                        saw_one = true;
                        blocks_catch_up_state.pending_blocks.push(next_block_hash);
                    }
                    if saw_one {
                        assert_eq!(
                            self.runtime_adapter.get_epoch_id_from_prev_block(&queued_block)?,
                            blocks_catch_up_state.epoch_id
                        );
                    }
                    blocks_catch_up_state.done_blocks.push(queued_block);
                }
                Err(_) => {
                    error!("Error processing block during catch up, retrying");
                    blocks_catch_up_state.pending_blocks.push(queued_block);
                }
            }
        }

        for pending_block in blocks_catch_up_state.pending_blocks.drain(..) {
            let block = self.store.get_block(&pending_block)?.clone();
            let prev_block = self.store.get_block(block.header().prev_hash())?.clone();

            let mut chain_update = self.chain_update();
            let work = chain_update.apply_chunks_preprocessing(
                me,
                &block,
                &prev_block,
                ApplyChunksMode::CatchingUp,
            )?;
            blocks_catch_up_state
                .scheduled_blocks
                .insert(pending_block, chain_update.into_saved_store_update());
            block_catch_up_scheduler(BlockCatchUpRequest {
                sync_hash: *sync_hash,
                block_hash: pending_block,
                work,
            });
        }

        Ok(())
    }

    fn block_catch_up_postprocess(
        &mut self,
        block_hash: &CryptoHash,
        results: Vec<Result<ApplyChunkResult, Error>>,
        saved_store_update: SavedStoreUpdate,
    ) -> Result<(), Error> {
        let block = self.store.get_block(block_hash)?.clone();
        let prev_block = self.store.get_block(block.header().prev_hash())?.clone();
        let mut chain_update = self.chain_update_from_save_store_update(saved_store_update);
        chain_update.apply_chunk_postprocessing(&block, &prev_block, results)?;
        chain_update.commit()?;
        Ok(())
    }

    /// Apply transactions in chunks for the next epoch in blocks that were blocked on the state sync
    pub fn finish_catchup_blocks(
        &mut self,
        me: &Option<AccountId>,
        epoch_first_block: &CryptoHash,
        block_accepted: &mut dyn FnMut(AcceptedBlock),
        block_misses_chunks: &mut dyn FnMut(BlockMissingChunks),
        orphan_misses_chunks: &mut dyn FnMut(OrphanMissingChunks),
        on_challenge: &mut dyn FnMut(ChallengeBody),
        affected_blocks: &Vec<CryptoHash>,
    ) -> Result<(), Error> {
        debug!(
            "Finishing catching up blocks after syncing pre {:?}, me: {:?}",
            epoch_first_block, me
        );

        let first_block = self.store.get_block(epoch_first_block)?.clone();

        let mut chain_store_update = ChainStoreUpdate::new(&mut self.store);

        // `blocks_to_catchup` consists of pairs (`prev_hash`, `hash`). For the block that precedes
        // `epoch_first_block` we should only remove the pair with hash = epoch_first_block, while
        // for all the blocks in the queue we can remove all the pairs that have them as `prev_hash`
        // since we processed all the blocks built on top of them above during the BFS
        chain_store_update
            .remove_block_to_catchup(*first_block.header().prev_hash(), *epoch_first_block);

        for block_hash in affected_blocks {
            debug!(target: "chain", "Catching up: removing prev={:?} from the queue. I'm {:?}", block_hash, me);
            chain_store_update.remove_prev_block_to_catchup(*block_hash);
        }
        chain_store_update.remove_state_dl_info(*epoch_first_block);

        chain_store_update.commit()?;

        for hash in affected_blocks.iter() {
            self.check_orphans(
                me,
                *hash,
                block_accepted,
                block_misses_chunks,
                orphan_misses_chunks,
                on_challenge,
            );
        }

        Ok(())
    }

    pub fn get_transaction_execution_result(
        &mut self,
        id: &CryptoHash,
    ) -> Result<Vec<ExecutionOutcomeWithIdView>, Error> {
        Ok(self.store.get_outcomes_by_id(id)?.into_iter().map(Into::into).collect())
    }

    fn get_recursive_transaction_results(
        &mut self,
        id: &CryptoHash,
    ) -> Result<Vec<ExecutionOutcomeWithIdView>, Error> {
        let outcome: ExecutionOutcomeWithIdView = self.get_execution_outcome(id)?.into();
        let receipt_ids = outcome.outcome.receipt_ids.clone();
        let mut results = vec![outcome];
        for receipt_id in &receipt_ids {
            results.extend(self.get_recursive_transaction_results(receipt_id)?);
        }
        Ok(results)
    }

    pub fn get_final_transaction_result(
        &mut self,
        transaction_hash: &CryptoHash,
    ) -> Result<FinalExecutionOutcomeView, Error> {
        let mut outcomes = self.get_recursive_transaction_results(transaction_hash)?;
        let mut looking_for_id = (*transaction_hash).into();
        let num_outcomes = outcomes.len();
        let status = outcomes
            .iter()
            .find_map(|outcome_with_id| {
                if outcome_with_id.id == looking_for_id {
                    match &outcome_with_id.outcome.status {
                        ExecutionStatusView::Unknown if num_outcomes == 1 => {
                            Some(FinalExecutionStatus::NotStarted)
                        }
                        ExecutionStatusView::Unknown => Some(FinalExecutionStatus::Started),
                        ExecutionStatusView::Failure(e) => {
                            Some(FinalExecutionStatus::Failure(e.clone()))
                        }
                        ExecutionStatusView::SuccessValue(v) => {
                            Some(FinalExecutionStatus::SuccessValue(v.clone()))
                        }
                        ExecutionStatusView::SuccessReceiptId(id) => {
                            looking_for_id = *id;
                            None
                        }
                    }
                } else {
                    None
                }
            })
            .expect("results should resolve to a final outcome");
        let receipts_outcome = outcomes.split_off(1);
        let transaction: SignedTransactionView = self
            .store
            .get_transaction(transaction_hash)?
            .ok_or_else(|| {
                ErrorKind::DBNotFoundErr(format!("Transaction {} is not found", transaction_hash))
            })?
            .clone()
            .into();
        let transaction_outcome = outcomes.pop().unwrap();
        Ok(FinalExecutionOutcomeView { status, transaction, transaction_outcome, receipts_outcome })
    }

    pub fn get_final_transaction_result_with_receipt(
        &mut self,
        final_outcome: FinalExecutionOutcomeView,
    ) -> Result<FinalExecutionOutcomeWithReceiptView, Error> {
        let receipt_id_from_transaction =
            final_outcome.transaction_outcome.outcome.receipt_ids.get(0).cloned();
        let is_local_receipt =
            final_outcome.transaction.signer_id == final_outcome.transaction.receiver_id;

        let receipts = final_outcome
            .receipts_outcome
            .iter()
            .filter_map(|outcome| {
                if Some(outcome.id) == receipt_id_from_transaction && is_local_receipt {
                    None
                } else {
                    Some(self.store.get_receipt(&outcome.id).and_then(|r| {
                        r.cloned().map(Into::into).ok_or_else(|| {
                            ErrorKind::DBNotFoundErr(format!("Receipt {} is not found", outcome.id))
                                .into()
                        })
                    }))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(FinalExecutionOutcomeWithReceiptView { final_outcome, receipts })
    }

    /// Find a validator to forward transactions to
    pub fn find_chunk_producer_for_forwarding(
        &self,
        epoch_id: &EpochId,
        shard_id: ShardId,
        horizon: BlockHeight,
    ) -> Result<AccountId, Error> {
        let head = self.head()?;
        let target_height = head.height + horizon - 1;
        self.runtime_adapter.get_chunk_producer(epoch_id, target_height, shard_id)
    }

    /// Find a validator that is responsible for a given shard to forward requests to
    pub fn find_validator_for_forwarding(&self, shard_id: ShardId) -> Result<AccountId, Error> {
        let head = self.head()?;
        let epoch_id = self.runtime_adapter.get_epoch_id_from_prev_block(&head.last_block_hash)?;
        self.find_chunk_producer_for_forwarding(&epoch_id, shard_id, TX_ROUTING_HEIGHT_HORIZON)
    }

    pub fn check_block_final_and_canonical(
        &mut self,
        block_hash: &CryptoHash,
    ) -> Result<(), Error> {
        let last_final_block_hash = *self.head_header()?.last_final_block();
        let last_final_height = self.get_block_header(&last_final_block_hash)?.height();
        let block_header = self.get_block_header(block_hash)?.clone();
        if block_header.height() <= last_final_height {
            self.is_on_current_chain(&block_header)
        } else {
            Err(ErrorKind::Other(format!("{} not on current chain", block_hash)).into())
        }
    }

    /// Get all execution outcomes generated when the chunk are applied
    pub fn get_block_execution_outcomes(
        &mut self,
        block_hash: &CryptoHash,
    ) -> Result<HashMap<ShardId, Vec<ExecutionOutcomeWithIdAndProof>>, Error> {
        let block = self.get_block(block_hash)?;
        let chunk_headers = block.chunks().iter().cloned().collect::<Vec<_>>();

        let mut res = HashMap::new();
        for chunk_header in chunk_headers {
            let shard_id = chunk_header.shard_id();
            let outcomes = self
                .mut_store()
                .get_outcomes_by_block_hash_and_shard_id(block_hash, shard_id)?
                .into_iter()
                .flat_map(|id| {
                    let mut outcomes =
                        self.store.get_outcomes_by_id(&id).unwrap_or_else(|_| vec![]);
                    outcomes.retain(|outcome| &outcome.block_hash == block_hash);
                    outcomes
                })
                .collect::<Vec<_>>();
            res.insert(shard_id, outcomes);
        }
        Ok(res)
    }
}

/// Implement block merkle proof retrieval.
impl Chain {
    fn combine_maybe_hashes(
        hash1: Option<MerkleHash>,
        hash2: Option<MerkleHash>,
    ) -> Option<MerkleHash> {
        match (hash1, hash2) {
            (Some(h1), Some(h2)) => Some(combine_hash(&h1, &h2)),
            (Some(h1), None) => Some(h1),
            (None, Some(_)) => {
                debug_assert!(false, "Inconsistent state in merkle proof computation: left node is None but right node exists");
                None
            }
            _ => None,
        }
    }

    fn chain_update(&mut self) -> ChainUpdate {
        ChainUpdate::new(
            &mut self.store,
            self.runtime_adapter.clone(),
            &self.orphans,
            &self.blocks_with_missing_chunks,
            self.epoch_length,
            &self.block_economics_config,
            self.doomslug_threshold_mode,
            &self.genesis,
            self.transaction_validity_period,
            self.pending_states_to_patch.take(),
        )
    }

    fn chain_update_from_save_store_update(
        &mut self,
        saved_store_update: SavedStoreUpdate,
    ) -> ChainUpdate {
        ChainUpdate::new_from_save_store_update(
            &mut self.store,
            saved_store_update,
            self.runtime_adapter.clone(),
            &self.orphans,
            &self.blocks_with_missing_chunks,
            self.epoch_length,
            &self.block_economics_config,
            self.doomslug_threshold_mode,
            &self.genesis,
            self.transaction_validity_period,
            self.pending_states_to_patch.take(),
        )
    }

    /// Get node at given position (index, level). If the node does not exist, return `None`.
    fn get_merkle_tree_node(
        &mut self,
        index: u64,
        level: u64,
        counter: u64,
        tree_size: u64,
        tree_nodes: &mut HashMap<(u64, u64), Option<MerkleHash>>,
    ) -> Result<Option<MerkleHash>, Error> {
        if let Some(hash) = tree_nodes.get(&(index, level)) {
            Ok(*hash)
        } else {
            if level == 0 {
                let maybe_hash = if index >= tree_size {
                    None
                } else {
                    Some(*self.mut_store().get_block_hash_from_ordinal(index)?)
                };
                tree_nodes.insert((index, level), maybe_hash);
                Ok(maybe_hash)
            } else {
                let cur_tree_size = (index + 1) * counter;
                let maybe_hash = if cur_tree_size > tree_size {
                    if index * counter <= tree_size {
                        let left_hash = self.get_merkle_tree_node(
                            index * 2,
                            level - 1,
                            counter / 2,
                            tree_size,
                            tree_nodes,
                        )?;
                        let right_hash = self.reconstruct_merkle_tree_node(
                            index * 2 + 1,
                            level - 1,
                            counter / 2,
                            tree_size,
                            tree_nodes,
                        )?;
                        Self::combine_maybe_hashes(left_hash, right_hash)
                    } else {
                        None
                    }
                } else {
                    Some(
                        *self
                            .mut_store()
                            .get_block_merkle_tree_from_ordinal(cur_tree_size)?
                            .get_path()
                            .last()
                            .ok_or_else(|| {
                                ErrorKind::Other("Merkle tree node missing".to_string())
                            })?,
                    )
                };
                tree_nodes.insert((index, level), maybe_hash);
                Ok(maybe_hash)
            }
        }
    }

    /// Reconstruct node at given position (index, level). If the node does not exist, return `None`.
    fn reconstruct_merkle_tree_node(
        &mut self,
        index: u64,
        level: u64,
        counter: u64,
        tree_size: u64,
        tree_nodes: &mut HashMap<(u64, u64), Option<MerkleHash>>,
    ) -> Result<Option<MerkleHash>, Error> {
        if let Some(hash) = tree_nodes.get(&(index, level)) {
            Ok(*hash)
        } else {
            if level == 0 {
                let maybe_hash = if index >= tree_size {
                    None
                } else {
                    Some(*self.mut_store().get_block_hash_from_ordinal(index)?)
                };
                tree_nodes.insert((index, level), maybe_hash);
                Ok(maybe_hash)
            } else {
                let left_hash = self.get_merkle_tree_node(
                    index * 2,
                    level - 1,
                    counter / 2,
                    tree_size,
                    tree_nodes,
                )?;
                let right_hash = self.reconstruct_merkle_tree_node(
                    index * 2 + 1,
                    level - 1,
                    counter / 2,
                    tree_size,
                    tree_nodes,
                )?;
                let maybe_hash = Self::combine_maybe_hashes(left_hash, right_hash);
                tree_nodes.insert((index, level), maybe_hash);

                Ok(maybe_hash)
            }
        }
    }

    /// Get merkle proof for block with hash `block_hash` in the merkle tree of `head_block_hash`.
    pub fn get_block_proof(
        &mut self,
        block_hash: &CryptoHash,
        head_block_hash: &CryptoHash,
    ) -> Result<MerklePath, Error> {
        let leaf_index = self.mut_store().get_block_merkle_tree(block_hash)?.size();
        let tree_size = self.mut_store().get_block_merkle_tree(head_block_hash)?.size();
        if leaf_index >= tree_size {
            if block_hash == head_block_hash {
                // special case if the block to prove is the same as head
                return Ok(vec![]);
            }
            return Err(ErrorKind::Other(format!(
                "block {} is ahead of head block {}",
                block_hash, head_block_hash
            ))
            .into());
        }
        let mut level = 0;
        let mut counter = 1;
        let mut cur_index = leaf_index;
        let mut path = vec![];
        let mut tree_nodes = HashMap::new();
        let mut iter = tree_size;
        while iter > 1 {
            if cur_index % 2 == 0 {
                cur_index += 1
            } else {
                cur_index -= 1;
            }
            let direction = if cur_index % 2 == 0 { Direction::Left } else { Direction::Right };
            let maybe_hash = if cur_index % 2 == 1 {
                // node not immediately available. Needs to be reconstructed
                self.reconstruct_merkle_tree_node(
                    cur_index,
                    level,
                    counter,
                    tree_size,
                    &mut tree_nodes,
                )?
            } else {
                self.get_merkle_tree_node(cur_index, level, counter, tree_size, &mut tree_nodes)?
            };
            if let Some(hash) = maybe_hash {
                path.push(MerklePathItem { hash, direction });
            }
            cur_index /= 2;
            iter = (iter + 1) / 2;
            level += 1;
            counter *= 2;
        }
        Ok(path)
    }
}

/// Various chain getters.
impl Chain {
    /// Gets chain head.
    #[inline]
    pub fn head(&self) -> Result<Tip, Error> {
        self.store.head()
    }

    /// Gets chain tail height
    #[inline]
    pub fn tail(&self) -> Result<BlockHeight, Error> {
        self.store.tail()
    }

    /// Gets chain header head.
    #[inline]
    pub fn header_head(&self) -> Result<Tip, Error> {
        self.store.header_head()
    }

    /// Header of the block at the head of the block chain (not the same thing as header_head).
    #[inline]
    pub fn head_header(&mut self) -> Result<&BlockHeader, Error> {
        self.store.head_header()
    }

    /// Get final head of the chain.
    #[inline]
    pub fn final_head(&self) -> Result<Tip, Error> {
        self.store.final_head()
    }

    /// Gets a block by hash.
    #[inline]
    pub fn get_block(&mut self, hash: &CryptoHash) -> Result<&Block, Error> {
        self.store.get_block(hash)
    }

    /// Gets a chunk from hash.
    #[inline]
    pub fn get_chunk(&mut self, chunk_hash: &ChunkHash) -> Result<&ShardChunk, Error> {
        self.store.get_chunk(chunk_hash)
    }

    /// Gets a chunk from header.
    #[inline]
    pub fn get_chunk_clone_from_header(
        &mut self,
        header: &ShardChunkHeader,
    ) -> Result<ShardChunk, Error> {
        self.store.get_chunk_clone_from_header(header)
    }

    /// Gets a block from the current chain by height.
    #[inline]
    pub fn get_block_by_height(&mut self, height: BlockHeight) -> Result<&Block, Error> {
        let hash = self.store.get_block_hash_by_height(height)?;
        self.store.get_block(&hash)
    }

    /// Gets block hash from the current chain by height.
    #[inline]
    pub fn get_block_hash_by_height(&mut self, height: BlockHeight) -> Result<CryptoHash, Error> {
        self.store.get_block_hash_by_height(height)
    }

    /// Gets a block header by hash.
    #[inline]
    pub fn get_block_header(&mut self, hash: &CryptoHash) -> Result<&BlockHeader, Error> {
        self.store.get_block_header(hash)
    }

    /// Returns block header from the canonical chain for given height if present.
    #[inline]
    pub fn get_header_by_height(&mut self, height: BlockHeight) -> Result<&BlockHeader, Error> {
        self.store.get_header_by_height(height)
    }

    /// Returns block header from the current chain defined by `sync_hash` for given height if present.
    #[inline]
    pub fn get_header_on_chain_by_height(
        &mut self,
        sync_hash: &CryptoHash,
        height: BlockHeight,
    ) -> Result<&BlockHeader, Error> {
        self.store.get_header_on_chain_by_height(sync_hash, height)
    }

    /// Get previous block header.
    #[inline]
    pub fn get_previous_header(&mut self, header: &BlockHeader) -> Result<&BlockHeader, Error> {
        self.store.get_previous_header(header)
    }

    /// Returns hash of the first available block after genesis.
    pub fn get_earliest_block_hash(&mut self) -> Result<Option<CryptoHash>, Error> {
        self.store.get_earliest_block_hash()
    }

    /// Check if block exists.
    #[inline]
    pub fn block_exists(&self, hash: &CryptoHash) -> Result<bool, Error> {
        self.store.block_exists(hash)
    }

    /// Get block extra that was computer after applying previous block.
    #[inline]
    pub fn get_block_extra(&mut self, block_hash: &CryptoHash) -> Result<&BlockExtra, Error> {
        self.store.get_block_extra(block_hash)
    }

    /// Get chunk extra that was computed after applying chunk with given hash.
    #[inline]
    pub fn get_chunk_extra(
        &mut self,
        block_hash: &CryptoHash,
        shard_uid: &ShardUId,
    ) -> Result<&ChunkExtra, Error> {
        self.store.get_chunk_extra(block_hash, shard_uid)
    }

    /// Get destination shard id for a given receipt id.
    #[inline]
    pub fn get_shard_id_for_receipt_id(
        &mut self,
        receipt_id: &CryptoHash,
    ) -> Result<&ShardId, Error> {
        self.store.get_shard_id_for_receipt_id(receipt_id)
    }

    /// Get next block hash for which there is a new chunk for the shard.
    /// If sharding changes before we can find a block with a new chunk for the shard,
    /// find the first block that contains a new chunk for any of the shards that split from the
    /// original shard
    pub fn get_next_block_hash_with_new_chunk(
        &mut self,
        block_hash: &CryptoHash,
        shard_id: ShardId,
    ) -> Result<Option<(CryptoHash, ShardId)>, Error> {
        let mut block_hash = *block_hash;
        let mut epoch_id = self.get_block_header(&block_hash)?.epoch_id().clone();
        let mut shard_layout = self.runtime_adapter.get_shard_layout(&epoch_id)?;
        // this corrects all the shard where the original shard will split to if sharding changes
        let mut shard_ids = vec![shard_id];

        while let Ok(next_block_hash) = self.store.get_next_block_hash(&block_hash) {
            let next_block_hash = *next_block_hash;
            let next_epoch_id = self.get_block_header(&next_block_hash)?.epoch_id().clone();
            if next_epoch_id != epoch_id {
                let next_shard_layout = self.runtime_adapter.get_shard_layout(&next_epoch_id)?;
                if next_shard_layout != shard_layout {
                    shard_ids = shard_ids
                        .into_iter()
                        .flat_map(|id| {
                            next_shard_layout.get_split_shard_ids(id).unwrap_or_else(|| {
                                panic!("invalid shard layout {:?} because it does not contain split shards for parent shard {}", next_shard_layout, id)
                            })
                        })
                        .collect();

                    shard_layout = next_shard_layout;
                }
                epoch_id = next_epoch_id;
            }
            block_hash = next_block_hash;

            let block = self.get_block(&block_hash)?;
            let chunks = block.chunks();
            for &shard_id in shard_ids.iter() {
                if chunks[shard_id as usize].height_included() == block.header().height() {
                    return Ok(Some((block_hash, shard_id)));
                }
            }
        }

        Ok(None)
    }

    /// Returns underlying ChainStore.
    #[inline]
    pub fn store(&self) -> &ChainStore {
        &self.store
    }

    /// Returns mutable ChainStore.
    #[inline]
    pub fn mut_store(&mut self) -> &mut ChainStore {
        &mut self.store
    }

    /// Returns underlying RuntimeAdapter.
    #[inline]
    pub fn runtime_adapter(&self) -> Arc<dyn RuntimeAdapter> {
        self.runtime_adapter.clone()
    }

    /// Returns genesis block.
    #[inline]
    pub fn genesis_block(&self) -> &Block {
        &self.genesis
    }

    /// Returns genesis block header.
    #[inline]
    pub fn genesis(&self) -> &BlockHeader {
        self.genesis.header()
    }

    /// Returns number of orphans currently in the orphan pool.
    #[inline]
    pub fn orphans_len(&self) -> usize {
        self.orphans.len()
    }

    /// Returns number of orphans currently in the orphan pool.
    #[inline]
    pub fn blocks_with_missing_chunks_len(&self) -> usize {
        self.blocks_with_missing_chunks.len()
    }

    /// Returns number of evicted orphans.
    #[inline]
    pub fn orphans_evicted_len(&self) -> usize {
        self.orphans.len_evicted()
    }

    /// Check if hash is for a known orphan.
    #[inline]
    pub fn is_orphan(&self, hash: &CryptoHash) -> bool {
        self.orphans.contains(hash)
    }

    /// Check if hash is for a known chunk orphan.
    #[inline]
    pub fn is_chunk_orphan(&self, hash: &CryptoHash) -> bool {
        self.blocks_with_missing_chunks.contains(hash)
    }

    /// Check if can sync with sync_hash
    pub fn check_sync_hash_validity(&mut self, sync_hash: &CryptoHash) -> Result<bool, Error> {
        let head = self.head()?;
        // It's important to check that Block exists because we will sync with it.
        // Do not replace with `get_block_header`.
        let sync_block = self.get_block(sync_hash)?;
        // The Epoch of sync_hash may be either the current one or the previous one
        if head.epoch_id == *sync_block.header().epoch_id()
            || head.epoch_id == *sync_block.header().next_epoch_id()
        {
            let prev_hash = *sync_block.header().prev_hash();
            // If sync_hash is not on the Epoch boundary, it's malicious behavior
            self.runtime_adapter.is_next_block_epoch_start(&prev_hash)
        } else {
            Ok(false) // invalid Epoch of sync_hash, possible malicious behavior
        }
    }

    /// Get transaction result for given hash of transaction or receipt id on the canonical chain
    pub fn get_execution_outcome(
        &mut self,
        id: &CryptoHash,
    ) -> Result<ExecutionOutcomeWithIdAndProof, Error> {
        let outcomes = self.store.get_outcomes_by_id(id)?;
        outcomes
            .into_iter()
            .find(|outcome| match self.get_block_header(&outcome.block_hash) {
                Ok(header) => {
                    let header = header.clone();
                    self.is_on_current_chain(&header).is_ok()
                }
                Err(_) => false,
            })
            .ok_or_else(|| ErrorKind::DBNotFoundErr(format!("EXECUTION OUTCOME: {}", id)).into())
    }

    /// Retrieve the up to `max_headers_returned` headers on the main chain
    /// `hashes`: a list of block "locators". `hashes` should be ordered from older blocks to
    ///           more recent blocks. This function will find the first block in `hashes`
    ///           that is on the main chain and returns the blocks after this block. If none of the
    ///           blocks in `hashes` are on the main chain, the function returns an empty vector.
    pub fn retrieve_headers(
        &mut self,
        hashes: Vec<CryptoHash>,
        max_headers_returned: u64,
        max_height: Option<BlockHeight>,
    ) -> Result<Vec<BlockHeader>, Error> {
        let header = match self.find_common_header(&hashes) {
            Some(header) => header,
            None => return Ok(vec![]),
        };

        let mut headers = vec![];
        let header_head_height = self.header_head()?.height;
        let max_height = max_height.unwrap_or(header_head_height);
        // TODO: this may be inefficient if there are a lot of skipped blocks.
        for h in header.height() + 1..=max_height {
            if let Ok(header) = self.get_header_by_height(h) {
                headers.push(header.clone());
                if headers.len() >= max_headers_returned as usize {
                    break;
                }
            }
        }
        Ok(headers)
    }

    /// Returns a vector of chunk headers, each of which corresponds to the previous chunk of
    /// a chunk in the block after `prev_block`
    /// This function is important when the block after `prev_block` has different number of chunks
    /// from `prev_block`.
    /// In block production and processing, often we need to get the previous chunks of chunks
    /// in the current block, this function provides a way to do so while handling sharding changes
    /// correctly.
    /// For example, if `prev_block` has two shards 0, 1 and the block after `prev_block` will have
    /// 4 shards 0, 1, 2, 3, 0 and 1 split from shard 0 and 2 and 3 split from shard 1.
    /// `get_prev_chunks(runtime_adapter, prev_block)` will return
    /// `[prev_block.chunks()[0], prev_block.chunks()[0], prev_block.chunks()[1], prev_block.chunks()[1]]`
    pub fn get_prev_chunk_headers(
        runtime_adapter: &dyn RuntimeAdapter,
        prev_block: &Block,
    ) -> Result<Vec<ShardChunkHeader>, Error> {
        let epoch_id = runtime_adapter.get_epoch_id_from_prev_block(prev_block.hash())?;
        let num_shards = runtime_adapter.num_shards(&epoch_id)?;
        let prev_shard_ids =
            runtime_adapter.get_prev_shard_ids(prev_block.hash(), (0..num_shards).collect())?;
        let chunks = prev_block.chunks();
        Ok(prev_shard_ids
            .into_iter()
            .map(|shard_id| chunks.get(shard_id as usize).unwrap().clone())
            .collect())
    }

    pub fn get_prev_chunk_header(
        runtime_adapter: &dyn RuntimeAdapter,
        prev_block: &Block,
        shard_id: ShardId,
    ) -> Result<ShardChunkHeader, Error> {
        let prev_shard_id =
            runtime_adapter.get_prev_shard_ids(prev_block.hash(), vec![shard_id])?[0];
        Ok(prev_block.chunks().get(prev_shard_id as usize).unwrap().clone())
    }

    pub fn group_receipts_by_shard(
        receipts: Vec<Receipt>,
        shard_layout: &ShardLayout,
    ) -> HashMap<ShardId, Vec<Receipt>> {
        let mut result = HashMap::with_capacity(shard_layout.num_shards() as usize);
        for receipt in receipts {
            let shard_id = account_id_to_shard_id(&receipt.receiver_id, shard_layout);
            let entry = result.entry(shard_id).or_insert_with(Vec::new);
            entry.push(receipt)
        }
        result
    }

    pub fn build_receipts_hashes(
        receipts: &[Receipt],
        shard_layout: &ShardLayout,
    ) -> Vec<CryptoHash> {
        if shard_layout.num_shards() == 1 {
            return vec![hash(&ReceiptList(0, receipts).try_to_vec().unwrap())];
        }
        let mut account_id_to_shard_id_map = HashMap::new();
        let mut shard_receipts: Vec<_> =
            (0..shard_layout.num_shards()).map(|i| (i, Vec::new())).collect();
        for receipt in receipts.iter() {
            let shard_id = match account_id_to_shard_id_map.get(&receipt.receiver_id) {
                Some(id) => *id,
                None => {
                    let id = account_id_to_shard_id(&receipt.receiver_id, shard_layout);
                    account_id_to_shard_id_map.insert(receipt.receiver_id.clone(), id);
                    id
                }
            };
            shard_receipts[shard_id as usize].1.push(receipt);
        }
        shard_receipts
            .into_iter()
            .map(|(i, rs)| {
                let bytes = (i, rs).try_to_vec().unwrap();
                hash(&bytes)
            })
            .collect()
    }
}

/// Sandbox node specific operations
#[cfg(feature = "sandbox")]
impl Chain {
    pub fn patch_state(&mut self, records: Vec<StateRecord>) {
        match self.pending_states_to_patch.take() {
            None => self.pending_states_to_patch = Some(records),
            Some(mut pending) => {
                pending.extend_from_slice(&records);
                self.pending_states_to_patch = Some(pending);
            }
        }
    }

    pub fn patch_state_in_progress(&self) -> bool {
        self.pending_states_to_patch.is_some()
    }
}

/// Chain update helper, contains information that is needed to process block
/// and decide to accept it or reject it.
/// If rejected nothing will be updated in underlying storage.
/// Safe to stop process mid way (Ctrl+C or crash).
pub struct ChainUpdate<'a> {
    runtime_adapter: Arc<dyn RuntimeAdapter>,
    chain_store_update: ChainStoreUpdate<'a>,
    orphans: &'a OrphanBlockPool,
    blocks_with_missing_chunks: &'a MissingChunksPool<Orphan>,
    epoch_length: BlockHeightDelta,
    block_economics_config: &'a BlockEconomicsConfig,
    doomslug_threshold_mode: DoomslugThresholdMode,
    genesis: &'a Block,
    #[allow(unused)]
    transaction_validity_period: BlockHeightDelta,
    states_to_patch: Option<Vec<StateRecord>>,
}

impl<'a> ChainAccess for ChainUpdate<'a> {
    fn orphans(&self) -> &'a OrphanBlockPool {
        &self.orphans
    }

    fn blocks_with_missing_chunks(&self) -> &'a MissingChunksPool<Orphan> {
        &self.blocks_with_missing_chunks
    }

    fn chain_store(&self) -> &dyn ChainStoreAccess {
        &self.chain_store_update
    }
}

pub struct SameHeightResult {
    shard_uid: ShardUId,
    gas_limit: Gas,
    apply_result: ApplyTransactionResult,
    apply_split_result_or_state_changes: Option<ApplySplitStateResultOrStateChanges>,
}

pub struct DifferentHeightResult {
    shard_uid: ShardUId,
    apply_result: ApplyTransactionResult,
    apply_split_result_or_state_changes: Option<ApplySplitStateResultOrStateChanges>,
}

pub struct SplitStateResult {
    // parent shard of the split states
    shard_uid: ShardUId,
    results: Vec<ApplySplitStateResult>,
}

pub enum ApplyChunkResult {
    SameHeight(SameHeightResult),
    DifferentHeight(DifferentHeightResult),
    SplitState(SplitStateResult),
}

impl<'a> ChainUpdate<'a> {
    pub fn new(
        store: &'a mut ChainStore,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        orphans: &'a OrphanBlockPool,
        blocks_with_missing_chunks: &'a MissingChunksPool<Orphan>,
        epoch_length: BlockHeightDelta,
        block_economics_config: &'a BlockEconomicsConfig,
        doomslug_threshold_mode: DoomslugThresholdMode,
        genesis: &'a Block,
        transaction_validity_period: BlockHeightDelta,
        states_to_patch: Option<Vec<StateRecord>>,
    ) -> Self {
        let chain_store_update: ChainStoreUpdate<'_> = store.store_update();
        <ChainUpdate<'a>>::new_impl(
            runtime_adapter,
            orphans,
            blocks_with_missing_chunks,
            epoch_length,
            block_economics_config,
            doomslug_threshold_mode,
            genesis,
            transaction_validity_period,
            states_to_patch,
            chain_store_update,
        )
    }

    pub fn new_from_save_store_update(
        store: &'a mut ChainStore,
        saved_store_update: SavedStoreUpdate,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        orphans: &'a OrphanBlockPool,
        blocks_with_missing_chunks: &'a MissingChunksPool<Orphan>,
        epoch_length: BlockHeightDelta,
        block_economics_config: &'a BlockEconomicsConfig,
        doomslug_threshold_mode: DoomslugThresholdMode,
        genesis: &'a Block,
        transaction_validity_period: BlockHeightDelta,
        states_to_patch: Option<Vec<StateRecord>>,
    ) -> Self {
        let chain_store_update = saved_store_update.restore(store);
        <ChainUpdate<'a>>::new_impl(
            runtime_adapter,
            orphans,
            blocks_with_missing_chunks,
            epoch_length,
            block_economics_config,
            doomslug_threshold_mode,
            genesis,
            transaction_validity_period,
            states_to_patch,
            chain_store_update,
        )
    }

    fn new_impl(
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        orphans: &'a OrphanBlockPool,
        blocks_with_missing_chunks: &'a MissingChunksPool<Orphan>,
        epoch_length: BlockHeightDelta,
        block_economics_config: &'a BlockEconomicsConfig,
        doomslug_threshold_mode: DoomslugThresholdMode,
        genesis: &'a Block,
        transaction_validity_period: BlockHeightDelta,
        states_to_patch: Option<Vec<StateRecord>>,
        chain_store_update: ChainStoreUpdate<'a>,
    ) -> Self {
        ChainUpdate {
            runtime_adapter,
            chain_store_update,
            orphans,
            blocks_with_missing_chunks,
            epoch_length,
            block_economics_config,
            doomslug_threshold_mode,
            genesis,
            transaction_validity_period,
            states_to_patch,
        }
    }

    /// Commit changes to the chain into the database.
    pub fn commit(self) -> Result<(), Error> {
        self.chain_store_update.commit()
    }

    /// Saves store update to preserve between passing work to other thread
    pub fn into_saved_store_update(self) -> SavedStoreUpdate {
        self.chain_store_update.into()
    }

    /// Process block header as part of "header first" block propagation.
    /// We validate the header but we do not store it or update header head
    /// based on this. We will update these once we get the block back after
    /// requesting it.
    pub fn process_block_header(
        &mut self,
        header: &BlockHeader,
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<(), Error> {
        debug!(target: "chain", "Process block header: {} at {}", header.hash(), header.height());

        check_known(self, header.hash())?.map_err(|e| Error::from(ErrorKind::BlockKnown(e)))?;
        self.validate_header(header, &Provenance::NONE, on_challenge)?;
        Ok(())
    }

    /// Find previous header or return Orphan error if not found.
    pub fn get_previous_header(&mut self, header: &BlockHeader) -> Result<&BlockHeader, Error> {
        self.chain_store_update.get_previous_header(header).map_err(|e| match e.kind() {
            ErrorKind::DBNotFoundErr(_) => ErrorKind::Orphan.into(),
            other => other.into(),
        })
    }

    fn care_about_any_shard_or_part(
        &mut self,
        me: &Option<AccountId>,
        parent_hash: CryptoHash,
    ) -> Result<bool, Error> {
        let epoch_id = self.runtime_adapter.get_epoch_id_from_prev_block(&parent_hash)?;
        for shard_id in 0..self.runtime_adapter.num_shards(&epoch_id)? {
            if self.runtime_adapter.cares_about_shard(me.as_ref(), &parent_hash, shard_id, true)
                || self.runtime_adapter.will_care_about_shard(
                    me.as_ref(),
                    &parent_hash,
                    shard_id,
                    true,
                )
            {
                return Ok(true);
            }
        }
        for part_id in 0..self.runtime_adapter.num_total_parts() {
            if &Some(self.runtime_adapter.get_part_owner(&parent_hash, part_id as u64)?) == me {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn ping_missing_chunks(
        &mut self,
        me: &Option<AccountId>,
        parent_hash: CryptoHash,
        block: &Block,
    ) -> Result<(), Error> {
        if !self.care_about_any_shard_or_part(me, parent_hash)? {
            return Ok(());
        }
        let mut missing = vec![];
        let height = block.header().height();
        for (shard_id, chunk_header) in block.chunks().iter().enumerate() {
            // Check if any chunks are invalid in this block.
            if let Some(encoded_chunk) =
                self.chain_store_update.is_invalid_chunk(&chunk_header.chunk_hash())?
            {
                let merkle_paths = Block::compute_chunk_headers_root(block.chunks().iter()).1;
                let chunk_proof = ChunkProofs {
                    block_header: block.header().try_to_vec().expect("Failed to serialize"),
                    merkle_proof: merkle_paths[shard_id].clone(),
                    chunk: MaybeEncodedShardChunk::Encoded(encoded_chunk.clone()),
                };
                return Err(ErrorKind::InvalidChunkProofs(Box::new(chunk_proof)).into());
            }
            let shard_id = shard_id as ShardId;
            if chunk_header.height_included() == height {
                let chunk_hash = chunk_header.chunk_hash();

                if let Err(_) =
                    self.chain_store_update.get_partial_chunk(&chunk_header.chunk_hash())
                {
                    missing.push(chunk_header.clone());
                } else if self.runtime_adapter.cares_about_shard(
                    me.as_ref(),
                    &parent_hash,
                    shard_id,
                    true,
                ) || self.runtime_adapter.will_care_about_shard(
                    me.as_ref(),
                    &parent_hash,
                    shard_id,
                    true,
                ) {
                    if let Err(_) = self.chain_store_update.get_chunk(&chunk_hash) {
                        missing.push(chunk_header.clone());
                    }
                }
            }
        }
        if !missing.is_empty() {
            return Err(ErrorKind::ChunksMissing(missing).into());
        }
        Ok(())
    }

    pub fn save_incoming_receipts_from_block(
        &mut self,
        me: &Option<AccountId>,
        block: &Block,
    ) -> Result<(), Error> {
        if !self.care_about_any_shard_or_part(me, *block.header().prev_hash())? {
            return Ok(());
        }
        let height = block.header().height();
        let mut receipt_proofs_by_shard_id = HashMap::new();

        for chunk_header in block.chunks().iter() {
            if chunk_header.height_included() == height {
                let partial_encoded_chunk =
                    self.chain_store_update.get_partial_chunk(&chunk_header.chunk_hash()).unwrap();
                for receipt in partial_encoded_chunk.receipts().iter() {
                    let ReceiptProof(_, shard_proof) = receipt;
                    let ShardProof { from_shard_id: _, to_shard_id, proof: _ } = shard_proof;
                    receipt_proofs_by_shard_id
                        .entry(*to_shard_id)
                        .or_insert_with(Vec::new)
                        .push(receipt.clone());
                }
            }
        }

        for (shard_id, mut receipt_proofs) in receipt_proofs_by_shard_id {
            let mut slice = [0u8; 32];
            slice.copy_from_slice(block.hash().as_ref());
            let mut rng: StdRng = SeedableRng::from_seed(slice);
            receipt_proofs.shuffle(&mut rng);
            self.chain_store_update.save_incoming_receipt(block.hash(), shard_id, receipt_proofs);
        }

        Ok(())
    }

    pub fn create_chunk_state_challenge(
        &mut self,
        prev_block: &Block,
        block: &Block,
        chunk_header: &ShardChunkHeader,
    ) -> Result<ChunkState, Error> {
        let chunk_shard_id = chunk_header.shard_id();
        let prev_chunk_header = &prev_block.chunks()[chunk_shard_id as usize];
        let prev_merkle_proofs = Block::compute_chunk_headers_root(prev_block.chunks().iter()).1;
        let merkle_proofs = Block::compute_chunk_headers_root(block.chunks().iter()).1;
        let prev_chunk = self
            .chain_store_update
            .get_chain_store()
            .get_chunk_clone_from_header(&prev_block.chunks()[chunk_shard_id as usize].clone())
            .unwrap();
        let receipt_proof_response: Vec<ReceiptProofResponse> =
            self.chain_store_update.get_incoming_receipts_for_shard(
                chunk_shard_id,
                *prev_block.hash(),
                prev_chunk_header.height_included(),
            )?;
        let receipts = collect_receipts_from_response(&receipt_proof_response);

        let challenges_result = self.verify_challenges(
            block.challenges(),
            block.header().epoch_id(),
            block.header().prev_hash(),
            Some(block.hash()),
        )?;
        let prev_chunk_inner = prev_chunk.cloned_header().take_inner();
        let is_first_block_with_chunk_of_version = check_if_block_is_first_with_chunk_of_version(
            &mut self.chain_store_update,
            self.runtime_adapter.as_ref(),
            prev_block.hash(),
            chunk_shard_id,
        )?;
        let apply_result = self
            .runtime_adapter
            .apply_transactions_with_optional_storage_proof(
                chunk_shard_id,
                prev_chunk_inner.prev_state_root(),
                prev_chunk.height_included(),
                prev_block.header().raw_timestamp(),
                prev_chunk_inner.prev_block_hash(),
                prev_block.hash(),
                &receipts,
                prev_chunk.transactions(),
                prev_chunk_inner.validator_proposals(),
                prev_block.header().gas_price(),
                prev_chunk_inner.gas_limit(),
                &challenges_result,
                *block.header().random_value(),
                true,
                true,
                is_first_block_with_chunk_of_version,
                None,
            )
            .unwrap();
        let partial_state = apply_result.proof.unwrap().nodes;
        Ok(ChunkState {
            prev_block_header: prev_block.header().try_to_vec()?,
            block_header: block.header().try_to_vec()?,
            prev_merkle_proof: prev_merkle_proofs[chunk_shard_id as usize].clone(),
            merkle_proof: merkle_proofs[chunk_shard_id as usize].clone(),
            prev_chunk,
            chunk_header: chunk_header.clone(),
            partial_state,
        })
    }

    /// Applies chunks and processes results
    fn apply_chunks_and_process_results(
        &mut self,
        block: &Block,
        prev_block: &Block,
        work: Vec<Box<dyn FnOnce() -> Result<ApplyChunkResult, Error> + Send + 'static>>,
    ) -> Result<(), Error> {
        let apply_results = do_apply_chunks(work);
        self.apply_chunk_postprocessing(block, prev_block, apply_results)
    }

    fn apply_chunk_postprocessing(
        &mut self,
        block: &Block,
        prev_block: &Block,
        apply_results: Vec<Result<ApplyChunkResult, Error>>,
    ) -> Result<(), Error> {
        apply_results.into_iter().try_for_each(|result| -> Result<(), Error> {
            self.process_apply_chunk_result(result?, *block.hash(), *prev_block.hash())
        })
    }

    fn get_split_state_roots(
        &mut self,
        block: &Block,
        shard_id: ShardId,
    ) -> Result<HashMap<ShardUId, StateRoot>, Error> {
        let next_shard_layout =
            self.runtime_adapter.get_shard_layout(block.header().next_epoch_id())?;
        let new_shards = next_shard_layout.get_split_shard_uids(shard_id).unwrap_or_else(|| {
            panic!("shard layout must contain maps of all shards to its split shards {}", shard_id)
        });
        new_shards
            .iter()
            .map(|shard_uid| {
                self.chain_store_update
                    .get_chunk_extra(block.header().prev_hash(), shard_uid)
                    .map(|chunk_extra| (*shard_uid, *chunk_extra.state_root()))
            })
            .collect()
    }

    /// Creates jobs that would apply chunks
    fn apply_chunks_preprocessing(
        &mut self,
        me: &Option<AccountId>,
        block: &Block,
        prev_block: &Block,
        mode: ApplyChunksMode,
    ) -> Result<Vec<Box<dyn FnOnce() -> Result<ApplyChunkResult, Error> + Send + 'static>>, Error>
    {
        let mut result: Vec<Box<dyn FnOnce() -> Result<ApplyChunkResult, Error> + Send + 'static>> =
            Vec::new();
        let challenges_result = self.verify_challenges(
            block.challenges(),
            block.header().epoch_id(),
            block.header().prev_hash(),
            Some(block.hash()),
        )?;
        self.chain_store_update.save_block_extra(block.hash(), BlockExtra { challenges_result });
        #[cfg(not(feature = "mock_network"))]
        let protocol_version =
            self.runtime_adapter.get_epoch_protocol_version(block.header().epoch_id())?;

        let prev_hash = block.header().prev_hash();
        let will_shard_layout_change =
            self.runtime_adapter.will_shard_layout_change_next_epoch(prev_hash)?;
        let prev_chunk_headers = Chain::get_prev_chunk_headers(&*self.runtime_adapter, prev_block)?;
        for (shard_id, (chunk_header, prev_chunk_header)) in
            (block.chunks().iter().zip(prev_chunk_headers.iter())).enumerate()
        {
            let shard_id = shard_id as ShardId;
            let cares_about_shard_this_epoch =
                self.runtime_adapter.cares_about_shard(me.as_ref(), prev_hash, shard_id, true);
            let cares_about_shard_next_epoch =
                self.runtime_adapter.will_care_about_shard(me.as_ref(), prev_hash, shard_id, true);
            // We want to guarantee that transactions are only applied once for each shard, even
            // though apply_chunks may be called twice, once with ApplyChunksMode::NotCaughtUp
            // once with ApplyChunksMode::CatchingUp
            // Note that this variable does not guard whether we split states or not, see the comments
            // before `need_to_split_state`
            let should_apply_transactions = match mode {
                // next epoch's shard states are not ready, only update this epoch's shards
                ApplyChunksMode::NotCaughtUp => cares_about_shard_this_epoch,
                // update both this epoch and next epoch
                ApplyChunksMode::IsCaughtUp => {
                    cares_about_shard_this_epoch || cares_about_shard_next_epoch
                }
                // catching up next epoch's shard states, do not update this epoch's shard state
                // since it has already been updated through ApplyChunksMode::NotCaughtUp
                ApplyChunksMode::CatchingUp => {
                    !cares_about_shard_this_epoch && cares_about_shard_next_epoch
                }
            };
            let need_to_split_states = will_shard_layout_change && cares_about_shard_next_epoch;
            // We can only split states when states are ready, i.e., mode != ApplyChunksMode::NotCaughtUp
            // 1) if should_apply_transactions == true && split_state_roots.is_some(),
            //     that means split states are ready.
            //    `apply_split_state_changes` will apply updates to split_states
            // 2) if should_apply_transactions == true && split_state_roots.is_none(),
            //     that means split states are not ready yet.
            //    `apply_split_state_changes` will return `state_changes_for_split_states`,
            //     which will be stored to the database in `process_apply_chunks`
            // 3) if should_apply_transactions == false && split_state_roots.is_some()
            //    This implies mode == CatchingUp and cares_about_shard_this_epoch == true,
            //    otherwise should_apply_transactions will be true
            //    That means transactions have already been applied last time when apply_chunks are
            //    called with mode NotCaughtUp, therefore `state_changes_for_split_states` have been
            //    stored in the database. Then we can safely read that and apply that to the split
            //    states
            let split_state_roots = if need_to_split_states {
                match mode {
                    ApplyChunksMode::IsCaughtUp | ApplyChunksMode::CatchingUp => {
                        Some(self.get_split_state_roots(block, shard_id)?)
                    }
                    ApplyChunksMode::NotCaughtUp => None,
                }
            } else {
                None
            };
            let shard_uid =
                self.runtime_adapter.shard_id_to_uid(shard_id, block.header().epoch_id())?;
            let is_new_chunk = chunk_header.height_included() == block.header().height();
            if should_apply_transactions {
                if is_new_chunk {
                    let prev_chunk_height_included = prev_chunk_header.height_included();
                    if cares_about_shard_this_epoch {
                        self.chain_store_update.save_receipt_id_to_shard_id(
                            &*self.runtime_adapter,
                            prev_hash,
                            shard_id,
                            prev_chunk_height_included,
                        )?;
                    }
                    // Validate state root.
                    let prev_chunk_extra =
                        self.chain_store_update.get_chunk_extra(prev_hash, &shard_uid)?.clone();

                    // Validate that all next chunk information matches previous chunk extra.
                    validate_chunk_with_chunk_extra(
                        // It's safe here to use ChainStore instead of ChainStoreUpdate
                        // because we're asking prev_chunk_header for already committed block
                        self.chain_store_update.get_chain_store(),
                        &*self.runtime_adapter,
                        block.header().prev_hash(),
                        &prev_chunk_extra,
                        prev_chunk_height_included,
                        chunk_header,
                    )
                    .map_err(|e| {
                        warn!(target: "chain", "Failed to validate chunk extra: {:?}.\n\
                                                block prev_hash: {}\n\
                                                block hash: {}\n\
                                                shard_id: {}\n\
                                                prev_chunk_height_included: {}\n\
                                                prev_chunk_extra: {:#?}\n\
                                                chunk_header: {:#?}", e,block.header().prev_hash(),block.header().hash(),shard_id,prev_chunk_height_included,prev_chunk_extra,chunk_header);
                        byzantine_assert!(false);
                        match self.create_chunk_state_challenge(prev_block, block, chunk_header) {
                            Ok(chunk_state) => {
                                Error::from(ErrorKind::InvalidChunkState(Box::new(chunk_state)))
                            }
                            Err(err) => err,
                        }
                    })?;
                    let receipt_proof_response: Vec<ReceiptProofResponse> =
                        self.chain_store_update.get_incoming_receipts_for_shard(
                            shard_id,
                            *block.hash(),
                            prev_chunk_height_included,
                        )?;
                    let receipts = collect_receipts_from_response(&receipt_proof_response);
                    let chunk = self
                        .chain_store_update
                        .get_chunk_clone_from_header(&chunk_header.clone())?;

                    let transactions = chunk.transactions();
                    if !validate_transactions_order(transactions) {
                        let merkle_paths =
                            Block::compute_chunk_headers_root(block.chunks().iter()).1;
                        let chunk_proof = ChunkProofs {
                            block_header: block.header().try_to_vec().expect("Failed to serialize"),
                            merkle_proof: merkle_paths[shard_id as usize].clone(),
                            chunk: MaybeEncodedShardChunk::Decoded(chunk),
                        };
                        return Err(Error::from(ErrorKind::InvalidChunkProofs(Box::new(
                            chunk_proof,
                        ))));
                    }

                    // if we are running mock_network, ignore this check because
                    // this check may require old block headers, which may not exist in storage
                    // of the client in the mock network
                    #[cfg(not(feature = "mock_network"))]
                    if checked_feature!("stable", AccessKeyNonceRange, protocol_version) {
                        let transaction_validity_period = self.transaction_validity_period;
                        for transaction in transactions {
                            self.chain_store_update
                                .get_chain_store()
                                .check_transaction_validity_period(
                                    prev_block.header(),
                                    &transaction.transaction.block_hash,
                                    transaction_validity_period,
                                )
                                .map_err(|_| Error::from(ErrorKind::InvalidTransactions))?;
                        }
                    };

                    let chunk_inner = chunk.cloned_header().take_inner();
                    let gas_limit = chunk_inner.gas_limit();

                    // This variable is responsible for checking to which block we can apply receipts previously lost in apply_chunks
                    // (see https://github.com/near/nearcore/pull/4248/)
                    // We take the first block with existing chunk in the first epoch in which protocol feature
                    // RestoreReceiptsAfterFixApplyChunks was enabled, and put the restored receipts there.
                    let is_first_block_with_chunk_of_version =
                        check_if_block_is_first_with_chunk_of_version(
                            &mut self.chain_store_update,
                            self.runtime_adapter.as_ref(),
                            prev_block.hash(),
                            shard_id,
                        )?;

                    let runtime_adapter = self.runtime_adapter.clone();
                    let block_hash = *block.hash();
                    let challenges_result = block.header().challenges_result().clone();
                    let block_timestamp = block.header().raw_timestamp();
                    let gas_price = prev_block.header().gas_price();
                    let random_seed = *block.header().random_value();
                    let height = chunk_header.height_included();
                    let prev_block_hash = chunk_header.prev_block_hash();
                    #[cfg(feature = "sandbox")]
                    let states_to_patch = self.states_to_patch.take();

                    result.push(Box::new(move || -> Result<ApplyChunkResult, Error> {
                        let _timer = CryptoHashTimer::new(chunk.chunk_hash().0);
                        match runtime_adapter.apply_transactions(
                            shard_id,
                            chunk_inner.prev_state_root(),
                            height,
                            block_timestamp,
                            &prev_block_hash,
                            &block_hash,
                            &receipts,
                            chunk.transactions(),
                            chunk_inner.validator_proposals(),
                            gas_price,
                            gas_limit,
                            &challenges_result,
                            random_seed,
                            true,
                            is_first_block_with_chunk_of_version,
                            #[cfg(feature = "sandbox")]
                            states_to_patch,
                            #[cfg(not(feature = "sandbox"))]
                            None,
                        ) {
                            Ok(apply_result) => {
                                let apply_split_result_or_state_changes =
                                    if will_shard_layout_change {
                                        Some(Self::apply_split_state_changes(
                                            &*runtime_adapter,
                                            &block_hash,
                                            &prev_block_hash,
                                            &apply_result,
                                            split_state_roots,
                                        )?)
                                    } else {
                                        None
                                    };
                                Ok(ApplyChunkResult::SameHeight(SameHeightResult {
                                    gas_limit,
                                    shard_uid,
                                    apply_result,
                                    apply_split_result_or_state_changes,
                                }))
                            }
                            Err(err) => Err(ErrorKind::Other(err.to_string()).into()),
                        }
                    }));
                } else {
                    let new_extra = self
                        .chain_store_update
                        .get_chunk_extra(prev_block.hash(), &shard_uid)?
                        .clone();

                    let runtime_adapter = self.runtime_adapter.clone();
                    let block_hash = *block.hash();
                    let challenges_result = block.header().challenges_result().clone();
                    let block_timestamp = block.header().raw_timestamp();
                    let gas_price = block.header().gas_price();
                    let random_seed = *block.header().random_value();
                    let height = block.header().height();
                    let prev_block_hash = *prev_block.hash();
                    #[cfg(feature = "sandbox")]
                    let states_to_patch = self.states_to_patch.take();
                    #[cfg(not(feature = "sandbox"))]
                    let _ = self.states_to_patch;

                    result.push(Box::new(move || -> Result<ApplyChunkResult, Error> {
                        match runtime_adapter.apply_transactions(
                            shard_id,
                            new_extra.state_root(),
                            height,
                            block_timestamp,
                            &prev_block_hash,
                            &block_hash,
                            &[],
                            &[],
                            new_extra.validator_proposals(),
                            gas_price,
                            new_extra.gas_limit(),
                            &challenges_result,
                            random_seed,
                            false,
                            false,
                            #[cfg(feature = "sandbox")]
                            states_to_patch,
                            #[cfg(not(feature = "sandbox"))]
                            None,
                        ) {
                            Ok(apply_result) => {
                                let apply_split_result_or_state_changes =
                                    if will_shard_layout_change {
                                        Some(Self::apply_split_state_changes(
                                            &*runtime_adapter,
                                            &block_hash,
                                            &prev_block_hash,
                                            &apply_result,
                                            split_state_roots,
                                        )?)
                                    } else {
                                        None
                                    };
                                Ok(ApplyChunkResult::DifferentHeight(DifferentHeightResult {
                                    shard_uid,
                                    apply_result,
                                    apply_split_result_or_state_changes,
                                }))
                            }
                            Err(err) => Err(ErrorKind::Other(err.to_string()).into()),
                        }
                    }));
                }
            } else if let Some(split_state_roots) = split_state_roots {
                // case 3)
                assert!(mode == ApplyChunksMode::CatchingUp && cares_about_shard_this_epoch);
                // Split state are ready. Read the state changes from the database and apply them
                // to the split states.
                let next_epoch_shard_layout =
                    self.runtime_adapter.get_shard_layout(block.header().next_epoch_id())?;
                let state_changes = self
                    .chain_store_update
                    .get_state_changes_for_split_states(block.hash(), shard_id)?;
                self.chain_store_update
                    .remove_state_changes_for_split_states(*block.hash(), shard_id);
                let runtime_adapter = self.runtime_adapter.clone();
                let block_hash = *block.hash();
                result.push(Box::new(move || -> Result<ApplyChunkResult, Error> {
                    Ok(ApplyChunkResult::SplitState(SplitStateResult {
                        shard_uid,
                        results: runtime_adapter.apply_update_to_split_states(
                            &block_hash,
                            split_state_roots,
                            &next_epoch_shard_layout,
                            state_changes,
                        )?,
                    }))
                }));
            }
        }

        Ok(result)
    }

    /// Process ApplyTransactionResult to apply changes to split states
    /// When shards will change next epoch,
    ///    if `split_state_roots` is not None, that means states for the split shards are ready
    ///    this function updates these states and return apply results for these states
    ///    otherwise, this function returns state changes needed to be applied to split
    ///    states. These state changes will be stored in the database by `process_split_state`
    fn apply_split_state_changes(
        runtime_adapter: &dyn RuntimeAdapter,
        block_hash: &CryptoHash,
        prev_block_hash: &CryptoHash,
        apply_result: &ApplyTransactionResult,
        split_state_roots: Option<HashMap<ShardUId, StateRoot>>,
    ) -> Result<ApplySplitStateResultOrStateChanges, Error> {
        let state_changes = StateChangesForSplitStates::from_raw_state_changes(
            apply_result.trie_changes.state_changes(),
            apply_result.processed_delayed_receipts.clone(),
        );
        let next_epoch_shard_layout = {
            let next_epoch_id =
                runtime_adapter.get_next_epoch_id_from_prev_block(prev_block_hash)?;
            runtime_adapter.get_shard_layout(&next_epoch_id)?
        };
        // split states are ready, apply update to them now
        if let Some(state_roots) = split_state_roots {
            let split_state_results = runtime_adapter.apply_update_to_split_states(
                block_hash,
                state_roots,
                &next_epoch_shard_layout,
                state_changes,
            )?;
            Ok(ApplySplitStateResultOrStateChanges::ApplySplitStateResults(split_state_results))
        } else {
            // split states are not ready yet, store state changes in consolidated_state_changes
            Ok(ApplySplitStateResultOrStateChanges::StateChangesForSplitStates(state_changes))
        }
    }

    /// Postprocess split state results or state changes, do the necessary update on chain
    /// for split state results: store the chunk extras and trie changes for the split states
    /// for state changes, store the state changes for splitting states
    fn process_split_state(
        &mut self,
        block_hash: &CryptoHash,
        prev_block_hash: &CryptoHash,
        shard_uid: &ShardUId,
        apply_results_or_state_changes: ApplySplitStateResultOrStateChanges,
    ) -> Result<(), Error> {
        match apply_results_or_state_changes {
            ApplySplitStateResultOrStateChanges::ApplySplitStateResults(results) => {
                // Split validator_proposals, gas_burnt, balance_burnt to each split shard
                // and store the chunk extra for split shards
                // Note that here we do not split outcomes by the new shard layout, we simply store
                // the outcome_root from the parent shard. This is because outcome proofs are
                // generated per shard using the old shard layout and stored in the database.
                // For these proofs to work, we must store the outcome root per shard
                // using the old shard layout instead of the new shard layout
                let chunk_extra = self.chain_store_update.get_chunk_extra(block_hash, shard_uid)?;
                let next_epoch_shard_layout = {
                    let epoch_id =
                        self.runtime_adapter.get_next_epoch_id_from_prev_block(prev_block_hash)?;
                    self.runtime_adapter.get_shard_layout(&epoch_id)?
                };

                let mut validator_proposals_by_shard: HashMap<_, Vec<_>> = HashMap::new();
                for validator_proposal in chunk_extra.validator_proposals() {
                    let shard_id = account_id_to_shard_uid(
                        validator_proposal.account_id(),
                        &next_epoch_shard_layout,
                    );
                    validator_proposals_by_shard
                        .entry(shard_id)
                        .or_default()
                        .push(validator_proposal);
                }

                let num_split_shards = next_epoch_shard_layout
                    .get_split_shard_uids(shard_uid.shard_id())
                    .unwrap_or_else(|| panic!("invalid shard layout {:?}", next_epoch_shard_layout))
                    .len() as NumShards;
                let total_gas_used = chunk_extra.gas_used();
                let total_balance_burnt = chunk_extra.balance_burnt();
                let gas_res = total_gas_used % num_split_shards;
                let gas_split = total_gas_used / num_split_shards;
                let balance_res = (total_balance_burnt % num_split_shards as u128) as NumShards;
                let balance_split = total_balance_burnt / (num_split_shards as u128);
                let gas_limit = chunk_extra.gas_limit();
                let outcome_root = *chunk_extra.outcome_root();

                let mut sum_gas_used = 0;
                let mut sum_balance_burnt = 0;
                for result in results {
                    let shard_id = result.shard_uid.shard_id();
                    let gas_burnt = gas_split + if shard_id < gas_res { 1 } else { 0 };
                    let balance_burnt = balance_split + if shard_id < balance_res { 1 } else { 0 };
                    let new_chunk_extra = ChunkExtra::new(
                        &result.new_root,
                        outcome_root,
                        validator_proposals_by_shard.remove(&result.shard_uid).unwrap_or_default(),
                        gas_burnt,
                        gas_limit,
                        balance_burnt,
                    );
                    sum_gas_used += gas_burnt;
                    sum_balance_burnt += balance_burnt;

                    self.chain_store_update.save_chunk_extra(
                        block_hash,
                        &result.shard_uid,
                        new_chunk_extra,
                    );
                    self.chain_store_update.save_trie_changes(result.trie_changes);
                }
                assert_eq!(sum_gas_used, total_gas_used);
                assert_eq!(sum_balance_burnt, total_balance_burnt);
            }
            ApplySplitStateResultOrStateChanges::StateChangesForSplitStates(state_changes) => {
                self.chain_store_update.add_state_changes_for_split_states(
                    *block_hash,
                    shard_uid.shard_id(),
                    state_changes,
                );
            }
        }
        Ok(())
    }

    /// Processed results of applying chunk
    fn process_apply_chunk_result(
        &mut self,
        result: ApplyChunkResult,
        block_hash: CryptoHash,
        prev_block_hash: CryptoHash,
    ) -> Result<(), Error> {
        match result {
            ApplyChunkResult::SameHeight(SameHeightResult {
                gas_limit,
                shard_uid,
                apply_result,
                apply_split_result_or_state_changes,
            }) => {
                let (outcome_root, outcome_paths) =
                    ApplyTransactionResult::compute_outcomes_proof(&apply_result.outcomes);
                let shard_id = shard_uid.shard_id();

                // Save state root after applying transactions.
                self.chain_store_update.save_chunk_extra(
                    &block_hash,
                    &shard_uid,
                    ChunkExtra::new(
                        &apply_result.new_root,
                        outcome_root,
                        apply_result.validator_proposals,
                        apply_result.total_gas_burnt,
                        gas_limit,
                        apply_result.total_balance_burnt,
                    ),
                );
                self.chain_store_update.save_trie_changes(apply_result.trie_changes);
                self.chain_store_update.save_outgoing_receipt(
                    &block_hash,
                    shard_id,
                    apply_result.outgoing_receipts,
                );
                // Save receipt and transaction results.
                self.chain_store_update.save_outcomes_with_proofs(
                    &block_hash,
                    shard_id,
                    apply_result.outcomes,
                    outcome_paths,
                );
                if let Some(apply_results_or_state_changes) = apply_split_result_or_state_changes {
                    self.process_split_state(
                        &block_hash,
                        &prev_block_hash,
                        &shard_uid,
                        apply_results_or_state_changes,
                    )?;
                }
            }
            ApplyChunkResult::DifferentHeight(DifferentHeightResult {
                shard_uid,
                apply_result,
                apply_split_result_or_state_changes,
            }) => {
                let mut new_extra =
                    self.chain_store_update.get_chunk_extra(&prev_block_hash, &shard_uid)?.clone();

                *new_extra.state_root_mut() = apply_result.new_root;

                self.chain_store_update.save_chunk_extra(&block_hash, &shard_uid, new_extra);
                self.chain_store_update.save_trie_changes(apply_result.trie_changes);

                if let Some(apply_results_or_state_changes) = apply_split_result_or_state_changes {
                    self.process_split_state(
                        &block_hash,
                        &prev_block_hash,
                        &shard_uid,
                        apply_results_or_state_changes,
                    )?;
                }
            }
            ApplyChunkResult::SplitState(SplitStateResult { shard_uid, results }) => {
                self.process_split_state(
                    &block_hash,
                    &prev_block_hash,
                    &shard_uid,
                    ApplySplitStateResultOrStateChanges::ApplySplitStateResults(results),
                )?;
            }
        };
        Ok(())
    }

    fn start_downloading_state(
        &mut self,
        me: &Option<AccountId>,
        block: &Block,
    ) -> Result<bool, Error> {
        let prev_hash = *block.header().prev_hash();
        let shards_to_dl =
            Chain::get_shards_to_dl_state(self.runtime_adapter.clone(), me, &prev_hash)?;
        let prev_block = self.chain_store_update.get_block(&prev_hash)?;

        if prev_block.chunks().len() != block.chunks().len() && !shards_to_dl.is_empty() {
            // Currently, the state sync algorithm assumes that the number of chunks do not change
            // between the epoch being synced to and the last epoch.
            // For example, if shard layout changes at the beginning of epoch T, validators
            // will not be able to sync states at epoch T for epoch T+1
            // Fortunately, since all validators track all shards for now, this error will not be
            // triggered in live yet
            // Instead of propagating the error, we simply log the error here because the error
            // do not affect processing blocks for this epoch. However, when the next epoch comes,
            // the validator will not have the states ready so it will halt.
            error!(
                "Cannot download states for epoch {:?} because sharding just changed. I'm {:?}",
                block.header().epoch_id(),
                me
            );
            debug_assert!(false);
        }
        if shards_to_dl.is_empty() {
            Ok(false)
        } else {
            debug!(target: "chain", "Downloading state for {:?}, I'm {:?}", shards_to_dl, me);

            let state_dl_info = StateSyncInfo {
                epoch_tail_hash: *block.header().hash(),
                shards: shards_to_dl
                    .iter()
                    .map(|shard_id| {
                        let chunk = &prev_block.chunks()[*shard_id as usize];
                        ShardInfo(*shard_id, chunk.chunk_hash())
                    })
                    .collect(),
            };

            self.chain_store_update.add_state_dl_info(state_dl_info);
            Ok(true)
        }
    }

    /// Runs the block processing, including validation and finding a place for the new block in the chain.
    /// Returns new head if chain head updated, as well as a boolean indicating if we need to start
    ///    fetching state for the next epoch.
    fn process_block(
        &mut self,
        me: &Option<AccountId>,
        block: &MaybeValidated<Block>,
        provenance: &Provenance,
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<Option<Tip>, Error> {
        let _span =
            tracing::debug_span!(target: "chain", "Process block", "#{}", block.header().height())
                .entered();
        debug!(target: "chain", "Block {}, approvals: {}, me: {:?}", block.hash(), block.header().num_approvals(), me);

        // Check that we know the epoch of the block before we try to get the header
        // (so that a block from unknown epoch doesn't get marked as an orphan)
        if !self.runtime_adapter.epoch_exists(block.header().epoch_id()) {
            return Err(ErrorKind::EpochOutOfBounds(block.header().epoch_id().clone()).into());
        }

        if block.chunks().len()
            != self.runtime_adapter.num_shards(block.header().epoch_id())? as usize
        {
            return Err(ErrorKind::IncorrectNumberOfChunkHeaders.into());
        }

        // Check if we have already processed this block previously.
        check_known(self, block.header().hash())?
            .map_err(|e| Error::from(ErrorKind::BlockKnown(e)))?;

        // Delay hitting the db for current chain head until we know this block is not already known.
        let head = self.chain_store_update.head()?;
        let is_next = block.header().prev_hash() == &head.last_block_hash;

        // Sandbox allows fast-forwarding, so only enable when not within sandbox
        if !cfg!(feature = "sandbox") {
            // A heuristic to prevent block height to jump too fast towards BlockHeight::max and cause
            // overflow-related problems
            let block_height = block.header().height();
            if block_height > head.height + self.epoch_length * 20 {
                return Err(ErrorKind::InvalidBlockHeight(block_height).into());
            }
        }

        // Block is an orphan if we do not know about the previous full block.
        if !is_next && !self.chain_store_update.block_exists(block.header().prev_hash())? {
            // Before we add the block to the orphan pool, do some checks:
            // 1. Block header is signed by the block producer for height.
            // 2. Chunk headers in block body match block header.
            // 3. Header has enough approvals from epoch block producers.
            // Not checked:
            // - Block producer could be slashed
            // - Chunk header signatures could be wrong
            if !self.partial_verify_orphan_header_signature(block.header())? {
                return Err(ErrorKind::InvalidSignature.into());
            }
            block.check_validity()?;
            // TODO: enable after #3729 and #3863
            // self.verify_orphan_header_approvals(&block.header())?;
            return Err(ErrorKind::Orphan.into());
        }

        // First real I/O expense.
        let prev = self.get_previous_header(block.header())?;
        let prev_hash = *prev.hash();
        let prev_prev_hash = *prev.prev_hash();
        let prev_gas_price = prev.gas_price();
        let prev_epoch_id = prev.epoch_id().clone();
        let prev_random_value = *prev.random_value();
        let prev_height = prev.height();

        // Do not accept old forks
        if prev_height < self.runtime_adapter.get_gc_stop_height(&head.last_block_hash) {
            return Err(ErrorKind::InvalidBlockHeight(prev_height).into());
        }

        let is_caught_up = if self.runtime_adapter.is_next_block_epoch_start(&prev_hash)? {
            if !self.prev_block_is_caught_up(&prev_prev_hash, &prev_hash)? {
                // The previous block is not caught up for the next epoch relative to the previous
                // block, which is the current epoch for this block, so this block cannot be applied
                // at all yet, needs to be orphaned
                return Err(ErrorKind::Orphan.into());
            }

            // For the first block of the epoch we check if we need to start download states for
            // shards that we will care about in the next epoch. If there is no state to be downloaded,
            // we consider that we are caught up, otherwise not
            let has_state_to_download = self.start_downloading_state(me, block)?;
            !has_state_to_download
        } else {
            self.prev_block_is_caught_up(&prev_prev_hash, &prev_hash)?
        };

        debug!(target: "chain", "{:?} Process block {}, is_caught_up: {}", me, block.hash(), is_caught_up);

        // Check the header is valid before we proceed with the full block.
        self.process_header_for_block(block.header(), provenance, on_challenge)?;

        self.runtime_adapter.verify_block_vrf(
            block.header().epoch_id(),
            block.header().height(),
            &prev_random_value,
            block.vrf_value(),
            block.vrf_proof(),
        )?;

        if block.header().random_value() != &hash(block.vrf_value().0.as_ref()) {
            return Err(ErrorKind::InvalidRandomnessBeaconOutput.into());
        }

        let res = block.validate_with(|block| {
            Chain::validate_block_impl(self.runtime_adapter.as_ref(), self.genesis, block)
                .map(|_| true)
        });
        if let Err(e) = res {
            byzantine_assert!(false);
            return Err(e.into());
        }

        let protocol_version =
            self.runtime_adapter.get_epoch_protocol_version(block.header().epoch_id())?;
        if !block.verify_gas_price(
            prev_gas_price,
            self.block_economics_config.min_gas_price(protocol_version),
            self.block_economics_config.max_gas_price(protocol_version),
            self.block_economics_config.gas_price_adjustment_rate(protocol_version),
        ) {
            byzantine_assert!(false);
            return Err(ErrorKind::InvalidGasPrice.into());
        }

        let prev_block = self.chain_store_update.get_block(&prev_hash)?.clone();

        self.ping_missing_chunks(me, prev_hash, block)?;
        self.save_incoming_receipts_from_block(me, block)?;

        // Do basic validation of chunks before applying the transactions
        let prev_chunk_headers =
            Chain::get_prev_chunk_headers(&*self.runtime_adapter, &prev_block)?;
        for (chunk_header, prev_chunk_header) in
            block.chunks().iter().zip(prev_chunk_headers.iter())
        {
            if chunk_header.height_included() == block.header().height() {
                if &chunk_header.prev_block_hash() != block.header().prev_hash() {
                    return Err(ErrorKind::InvalidChunk.into());
                }
            } else {
                if prev_chunk_header != chunk_header {
                    return Err(ErrorKind::InvalidChunk.into());
                }
            }
        }

        // If we have the state for shards in the next epoch already downloaded, apply the state transition
        // for these states as well
        // otherwise put the block into the permanent storage, waiting for be caught up
        let apply_chunk_work = if is_caught_up {
            self.apply_chunks_preprocessing(me, block, &prev_block, ApplyChunksMode::IsCaughtUp)?
        } else {
            debug!("Add block to catch up {:?} {:?}", prev_hash, *block.hash());
            self.chain_store_update.add_block_to_catchup(prev_hash, *block.hash());
            self.apply_chunks_preprocessing(me, block, &prev_block, ApplyChunksMode::NotCaughtUp)?
        };

        self.apply_chunks_and_process_results(block, &prev_block, apply_chunk_work)?;

        // Verify that proposals from chunks match block header proposals.
        let block_height = block.header().height();
        for pair in block
            .chunks()
            .iter()
            .filter(|chunk| block_height == chunk.height_included())
            .flat_map(|chunk| chunk.validator_proposals())
            .zip_longest(block.header().validator_proposals())
        {
            match pair {
                itertools::EitherOrBoth::Both(cp, hp) => {
                    if hp != cp {
                        // Proposals differed!
                        return Err(ErrorKind::InvalidValidatorProposals.into());
                    }
                }
                _ => {
                    // Can only occur if there were a different number of proposals in the header
                    // and chunks
                    return Err(ErrorKind::InvalidValidatorProposals.into());
                }
            }
        }

        // If block checks out, record validator proposals for given block.
        let last_final_block = block.header().last_final_block();
        let last_finalized_height = if last_final_block == &CryptoHash::default() {
            self.chain_store_update.get_genesis_height()
        } else {
            self.chain_store_update.get_block_header(last_final_block)?.height()
        };

        let epoch_manager_update = self
            .runtime_adapter
            .add_validator_proposals(BlockHeaderInfo::new(block.header(), last_finalized_height))?;
        self.chain_store_update.merge(epoch_manager_update);

        // Add validated block to the db, even if it's not the canonical fork.
        self.chain_store_update.save_block(block.clone().into_inner());
        self.chain_store_update.inc_block_refcount(block.header().prev_hash())?;

        // Update the chain head if it's the new tip
        let res = self.update_head(block.header())?;

        if res.is_some() {
            // On the epoch switch record the epoch light client block
            // Note that we only do it if `res.is_some()`, i.e. if the current block is the head.
            // This is necessary because the computation of the light client block relies on
            // `ColNextBlockHash`-es populated, and they are only populated for the canonical
            // chain. We need to be careful to avoid a situation when the first block of the epoch
            // never becomes a tip of the canonical chain.
            // Presently the epoch boundary is defined by the height, and the fork choice rule
            // is also just height, so the very first block to cross the epoch end is guaranteed
            // to be the head of the chain, and result in the light client block produced.
            if block.header().epoch_id() != &prev_epoch_id {
                let prev = self.get_previous_header(block.header())?.clone();
                if prev.last_final_block() != &CryptoHash::default() {
                    let light_client_block = self.create_light_client_block(&prev)?;
                    self.chain_store_update
                        .save_epoch_light_client_block(&prev_epoch_id.0, light_client_block);
                }
            }
        }

        Ok(res)
    }

    pub fn create_light_client_block(
        &mut self,
        header: &BlockHeader,
    ) -> Result<LightClientBlockView, Error> {
        // First update the last next_block, since it might not be set yet
        self.chain_store_update.save_next_block_hash(header.prev_hash(), *header.hash());

        Chain::create_light_client_block(
            header,
            &*self.runtime_adapter,
            &mut self.chain_store_update,
        )
    }

    fn prev_block_is_caught_up(
        &self,
        prev_prev_hash: &CryptoHash,
        prev_hash: &CryptoHash,
    ) -> Result<bool, Error> {
        // Needs to be used with care: for the first block of each epoch the semantic is slightly
        // different, since the prev_block is in a different epoch. So for all the blocks but the
        // first one in each epoch this method returns true if the block is ready to have state
        // applied for the next epoch, while for the first block in a particular epoch this method
        // returns true if the block is ready to have state applied for the current epoch (and
        // otherwise should be orphaned)
        Ok(!self.chain_store_update.get_blocks_to_catchup(prev_prev_hash)?.contains(prev_hash))
    }

    /// Process a block header as part of processing a full block.
    /// We want to be sure the header is valid before processing the full block.
    fn process_header_for_block(
        &mut self,
        header: &BlockHeader,
        provenance: &Provenance,
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<(), Error> {
        self.validate_header(header, provenance, on_challenge)?;
        self.chain_store_update.save_block_header(header.clone())?;
        self.update_header_head_if_not_challenged(header)?;
        Ok(())
    }

    fn validate_header(
        &mut self,
        header: &BlockHeader,
        provenance: &Provenance,
        on_challenge: &mut dyn FnMut(ChallengeBody),
    ) -> Result<(), Error> {
        // Refuse blocks from the too distant future.
        if header.timestamp() > Clock::utc() + Duration::seconds(ACCEPTABLE_TIME_DIFFERENCE) {
            return Err(ErrorKind::InvalidBlockFutureTime(header.timestamp()).into());
        }

        // First I/O cost, delay as much as possible.
        if !self.runtime_adapter.verify_header_signature(header)? {
            return Err(ErrorKind::InvalidSignature.into());
        }

        // Check we don't know a block with given height already.
        // If we do - send out double sign challenge and keep going as double signed blocks are valid blocks.
        if let Ok(epoch_id_to_blocks) = self
            .chain_store_update
            .get_chain_store()
            .get_all_block_hashes_by_height(header.height())
            .map(Clone::clone)
        {
            // Check if there is already known block of the same height that has the same epoch id
            if let Some(block_hashes) = epoch_id_to_blocks.get(header.epoch_id()) {
                // This should be guaranteed but it doesn't hurt to check again
                if !block_hashes.contains(header.hash()) {
                    let other_header = self
                        .chain_store_update
                        .get_block_header(block_hashes.iter().next().unwrap())?;

                    on_challenge(ChallengeBody::BlockDoubleSign(BlockDoubleSign {
                        left_block_header: header.try_to_vec().expect("Failed to serialize"),
                        right_block_header: other_header.try_to_vec().expect("Failed to serialize"),
                    }));
                }
            }
        }

        let prev_header = self.get_previous_header(header)?.clone();

        // Check that epoch_id in the header does match epoch given previous header (only if previous header is present).
        let epoch_id_from_prev_block =
            &self.runtime_adapter.get_epoch_id_from_prev_block(header.prev_hash())?;
        let epoch_id_from_header = header.epoch_id();
        if epoch_id_from_prev_block != epoch_id_from_header {
            return Err(ErrorKind::InvalidEpochHash.into());
        }

        // Check that epoch_id in the header does match epoch given previous header (only if previous header is present).
        if &self.runtime_adapter.get_next_epoch_id_from_prev_block(header.prev_hash())?
            != header.next_epoch_id()
        {
            return Err(ErrorKind::InvalidEpochHash.into());
        }

        if header.epoch_id() == prev_header.epoch_id() {
            if header.next_bp_hash() != prev_header.next_bp_hash() {
                return Err(ErrorKind::InvalidNextBPHash.into());
            }
        } else {
            if header.next_bp_hash()
                != &Chain::compute_bp_hash(
                    &*self.runtime_adapter,
                    header.next_epoch_id().clone(),
                    header.epoch_id().clone(),
                    header.prev_hash(),
                )?
            {
                return Err(ErrorKind::InvalidNextBPHash.into());
            }
        }

        if header.chunk_mask().len() as u64 != self.runtime_adapter.num_shards(header.epoch_id())? {
            return Err(ErrorKind::InvalidChunkMask.into());
        }

        if !header.verify_chunks_included() {
            return Err(ErrorKind::InvalidChunkMask.into());
        }

        if let Some(prev_height) = header.prev_height() {
            if prev_height != prev_header.height() {
                return Err(ErrorKind::Other("Invalid prev_height".to_string()).into());
            }
        }

        // Prevent time warp attacks and some timestamp manipulations by forcing strict
        // time progression.
        if header.raw_timestamp() <= prev_header.raw_timestamp() {
            return Err(ErrorKind::InvalidBlockPastTime(
                prev_header.timestamp(),
                header.timestamp(),
            )
            .into());
        }
        // If this is not the block we produced (hence trust in it) - validates block
        // producer, confirmation signatures and finality info.
        if *provenance != Provenance::PRODUCED {
            // first verify aggregated signature
            if !self.runtime_adapter.verify_approval(
                prev_header.hash(),
                prev_header.height(),
                header.height(),
                header.approvals(),
            )? {
                return Err(ErrorKind::InvalidApprovals.into());
            };

            let stakes = self
                .runtime_adapter
                .get_epoch_block_approvers_ordered(header.prev_hash())?
                .iter()
                .map(|(x, is_slashed)| (x.stake_this_epoch, x.stake_next_epoch, *is_slashed))
                .collect();
            if !Doomslug::can_approved_block_be_produced(
                self.doomslug_threshold_mode,
                header.approvals(),
                &stakes,
            ) {
                return Err(ErrorKind::NotEnoughApprovals.into());
            }

            let expected_last_ds_final_block = if prev_header.height() + 1 == header.height() {
                prev_header.hash()
            } else {
                prev_header.last_ds_final_block()
            };

            let expected_last_final_block = if prev_header.height() + 1 == header.height()
                && prev_header.last_ds_final_block() == prev_header.prev_hash()
            {
                prev_header.prev_hash()
            } else {
                prev_header.last_final_block()
            };

            if header.last_ds_final_block() != expected_last_ds_final_block
                || header.last_final_block() != expected_last_final_block
            {
                return Err(ErrorKind::InvalidFinalityInfo.into());
            }

            let mut block_merkle_tree =
                self.chain_store_update.get_block_merkle_tree(header.prev_hash())?.clone();
            block_merkle_tree.insert(*header.prev_hash());
            if &block_merkle_tree.root() != header.block_merkle_root() {
                return Err(ErrorKind::InvalidBlockMerkleRoot.into());
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    fn verify_orphan_header_approvals(&mut self, header: &BlockHeader) -> Result<(), Error> {
        let prev_hash = header.prev_hash();
        let prev_height = match header.prev_height() {
            None => {
                // this will accept orphans of V1 and V2
                // TODO: reject header V1 and V2 after a certain height
                return Ok(());
            }
            Some(prev_height) => prev_height,
        };
        let height = header.height();
        let epoch_id = header.epoch_id();
        let approvals = header.approvals();
        self.runtime_adapter.verify_approvals_and_threshold_orphan(
            epoch_id,
            self.doomslug_threshold_mode,
            prev_hash,
            prev_height,
            height,
            approvals,
        )
    }

    /// Update the header head if this header has most work.
    fn update_header_head_if_not_challenged(
        &mut self,
        header: &BlockHeader,
    ) -> Result<Option<Tip>, Error> {
        let header_head = self.chain_store_update.header_head()?;
        if header.height() > header_head.height {
            let tip = Tip::from_header(header);
            self.chain_store_update.save_header_head_if_not_challenged(&tip)?;
            debug!(target: "chain", "Header head updated to {} at {}", tip.last_block_hash, tip.height);
            metrics::HEADER_HEAD_HEIGHT.set(tip.height as i64);

            Ok(Some(tip))
        } else {
            Ok(None)
        }
    }

    fn update_final_head_from_block(&mut self, header: &BlockHeader) -> Result<Option<Tip>, Error> {
        let final_head = self.chain_store_update.final_head()?;
        let last_final_block_header =
            match self.chain_store_update.get_block_header(header.last_final_block()) {
                Ok(final_header) => final_header,
                Err(e) => match e.kind() {
                    ErrorKind::DBNotFoundErr(_) => return Ok(None),
                    _ => return Err(e),
                },
            };
        if last_final_block_header.height() > final_head.height {
            let tip = Tip::from_header(last_final_block_header);
            self.chain_store_update.save_final_head(&tip)?;
            Ok(Some(tip))
        } else {
            Ok(None)
        }
    }

    /// Directly updates the head if we've just appended a new block to it or handle
    /// the situation where the block has higher height to have a fork
    fn update_head(&mut self, header: &BlockHeader) -> Result<Option<Tip>, Error> {
        // if we made a fork with higher height than the head (which should also be true
        // when extending the head), update it
        self.update_final_head_from_block(header)?;
        let head = self.chain_store_update.head()?;
        if header.height() > head.height {
            let tip = Tip::from_header(header);

            self.chain_store_update.save_body_head(&tip)?;
            metrics::BLOCK_HEIGHT_HEAD.set(tip.height as i64);
            debug!(target: "chain", "Head updated to {} at {}", tip.last_block_hash, tip.height);
            Ok(Some(tip))
        } else {
            Ok(None)
        }
    }

    /// Marks a block as invalid,
    fn mark_block_as_challenged(
        &mut self,
        block_hash: &CryptoHash,
        challenger_hash: Option<&CryptoHash>,
    ) -> Result<(), Error> {
        info!(target: "chain", "Marking {} as challenged block (challenged in {:?}) and updating the chain.", block_hash, challenger_hash);
        let block_header = match self.chain_store_update.get_block_header(block_hash) {
            Ok(block_header) => block_header.clone(),
            Err(e) => match e.kind() {
                ErrorKind::DBNotFoundErr(_) => {
                    // The block wasn't seen yet, still challenge is good.
                    self.chain_store_update.save_challenged_block(*block_hash);
                    return Ok(());
                }
                _ => return Err(e),
            },
        };

        let cur_block_at_same_height =
            match self.chain_store_update.get_block_hash_by_height(block_header.height()) {
                Ok(bh) => Some(bh),
                Err(e) => match e.kind() {
                    ErrorKind::DBNotFoundErr(_) => None,
                    _ => return Err(e),
                },
            };

        self.chain_store_update.save_challenged_block(*block_hash);

        // If the block being invalidated is on the canonical chain, update head
        if cur_block_at_same_height == Some(*block_hash) {
            // We only consider two candidates for the new head: the challenger and the block
            //   immediately preceding the block being challenged
            // It could be that there is a better chain known. However, it is extremely unlikely,
            //   and even if there's such chain available, the very next block built on it will
            //   bring this node's head to that chain.
            let prev_header =
                self.chain_store_update.get_block_header(block_header.prev_hash())?.clone();
            let prev_height = prev_header.height();
            let new_head_header = if let Some(hash) = challenger_hash {
                let challenger_header = self.chain_store_update.get_block_header(hash)?;
                if challenger_header.height() > prev_height {
                    challenger_header
                } else {
                    &prev_header
                }
            } else {
                &prev_header
            };
            let last_final_block = *new_head_header.last_final_block();

            let tip = Tip::from_header(new_head_header);
            self.chain_store_update.save_head(&tip)?;
            let new_final_header =
                self.chain_store_update.get_block_header(&last_final_block)?.clone();
            self.chain_store_update.save_final_head(&Tip::from_header(&new_final_header))?;
        }

        Ok(())
    }

    pub fn set_state_finalize(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        shard_state_header: ShardStateSyncResponseHeader,
    ) -> Result<(), Error> {
        let (chunk, incoming_receipts_proofs) = match shard_state_header {
            ShardStateSyncResponseHeader::V1(shard_state_header) => (
                ShardChunk::V1(shard_state_header.chunk),
                shard_state_header.incoming_receipts_proofs,
            ),
            ShardStateSyncResponseHeader::V2(shard_state_header) => {
                (shard_state_header.chunk, shard_state_header.incoming_receipts_proofs)
            }
        };

        let block_header = self
            .chain_store_update
            .get_header_on_chain_by_height(&sync_hash, chunk.height_included())?
            .clone();

        // Getting actual incoming receipts.
        let mut receipt_proof_response: Vec<ReceiptProofResponse> = vec![];
        for incoming_receipt_proof in incoming_receipts_proofs.iter() {
            let ReceiptProofResponse(hash, _) = incoming_receipt_proof;
            let block_header = self.chain_store_update.get_block_header(hash)?;
            if block_header.height() <= chunk.height_included() {
                receipt_proof_response.push(incoming_receipt_proof.clone());
            }
        }
        let receipts = collect_receipts_from_response(&receipt_proof_response);
        // Prev block header should be present during state sync, since headers have been synced at this point.
        let gas_price = if block_header.height() == self.chain_store_update.get_genesis_height() {
            block_header.gas_price()
        } else {
            self.chain_store_update.get_block_header(block_header.prev_hash())?.gas_price()
        };

        let chunk_header = chunk.cloned_header();
        let gas_limit = chunk_header.gas_limit();
        let is_first_block_with_chunk_of_version = check_if_block_is_first_with_chunk_of_version(
            &mut self.chain_store_update,
            self.runtime_adapter.as_ref(),
            &chunk_header.prev_block_hash(),
            shard_id,
        )?;

        let apply_result = self.runtime_adapter.apply_transactions(
            shard_id,
            &chunk_header.prev_state_root(),
            chunk_header.height_included(),
            block_header.raw_timestamp(),
            &chunk_header.prev_block_hash(),
            block_header.hash(),
            &receipts,
            chunk.transactions(),
            chunk_header.validator_proposals(),
            gas_price,
            gas_limit,
            block_header.challenges_result(),
            *block_header.random_value(),
            true,
            is_first_block_with_chunk_of_version,
            None,
        )?;

        let (outcome_root, outcome_proofs) =
            ApplyTransactionResult::compute_outcomes_proof(&apply_result.outcomes);

        self.chain_store_update.save_chunk(chunk);

        self.chain_store_update.save_trie_changes(apply_result.trie_changes);
        let chunk_extra = ChunkExtra::new(
            &apply_result.new_root,
            outcome_root,
            apply_result.validator_proposals,
            apply_result.total_gas_burnt,
            gas_limit,
            apply_result.total_balance_burnt,
        );
        let shard_uid = self.runtime_adapter.shard_id_to_uid(shard_id, block_header.epoch_id())?;
        self.chain_store_update.save_chunk_extra(block_header.hash(), &shard_uid, chunk_extra);

        self.chain_store_update.save_outgoing_receipt(
            block_header.hash(),
            shard_id,
            apply_result.outgoing_receipts,
        );
        // Saving transaction results.
        self.chain_store_update.save_outcomes_with_proofs(
            block_header.hash(),
            shard_id,
            apply_result.outcomes,
            outcome_proofs,
        );
        // Saving all incoming receipts.
        for receipt_proof_response in incoming_receipts_proofs {
            self.chain_store_update.save_incoming_receipt(
                &receipt_proof_response.0,
                shard_id,
                receipt_proof_response.1,
            );
        }
        Ok(())
    }

    pub fn set_state_finalize_on_height(
        &mut self,
        height: BlockHeight,
        shard_id: ShardId,
        sync_hash: CryptoHash,
    ) -> Result<bool, Error> {
        let block_header_result =
            self.chain_store_update.get_header_on_chain_by_height(&sync_hash, height);
        if let Err(_) = block_header_result {
            // No such height, go ahead.
            return Ok(true);
        }
        let block_header = block_header_result?.clone();
        if block_header.hash() == &sync_hash {
            // Don't continue
            return Ok(false);
        }
        let prev_block_header =
            self.chain_store_update.get_block_header(block_header.prev_hash())?.clone();

        let shard_uid = self.runtime_adapter.shard_id_to_uid(shard_id, block_header.epoch_id())?;
        let mut chunk_extra =
            self.chain_store_update.get_chunk_extra(prev_block_header.hash(), &shard_uid)?.clone();

        let apply_result = self.runtime_adapter.apply_transactions(
            shard_id,
            chunk_extra.state_root(),
            block_header.height(),
            block_header.raw_timestamp(),
            prev_block_header.hash(),
            block_header.hash(),
            &[],
            &[],
            chunk_extra.validator_proposals(),
            prev_block_header.gas_price(),
            chunk_extra.gas_limit(),
            block_header.challenges_result(),
            *block_header.random_value(),
            false,
            false,
            None,
        )?;

        self.chain_store_update.save_trie_changes(apply_result.trie_changes);
        *chunk_extra.state_root_mut() = apply_result.new_root;

        self.chain_store_update.save_chunk_extra(block_header.hash(), &shard_uid, chunk_extra);
        Ok(true)
    }

    /// Returns correct / malicious challenges or Error if any challenge is invalid.
    pub fn verify_challenges(
        &mut self,
        challenges: &Vec<Challenge>,
        epoch_id: &EpochId,
        prev_block_hash: &CryptoHash,
        block_hash: Option<&CryptoHash>,
    ) -> Result<ChallengesResult, Error> {
        debug!(target: "chain", "Verifying challenges {:?}", challenges);
        let mut result = vec![];
        for challenge in challenges.iter() {
            match validate_challenge(&*self.runtime_adapter, epoch_id, prev_block_hash, challenge) {
                Ok((hash, account_ids)) => {
                    let is_double_sign = match challenge.body {
                        // If it's double signed block, we don't invalidate blocks just slash.
                        ChallengeBody::BlockDoubleSign(_) => true,
                        _ => {
                            self.mark_block_as_challenged(&hash, block_hash)?;
                            false
                        }
                    };
                    let slash_validators: Vec<_> = account_ids
                        .into_iter()
                        .map(|id| SlashedValidator::new(id, is_double_sign))
                        .collect();
                    result.extend(slash_validators);
                }
                Err(ref err) if err.kind() == ErrorKind::MaliciousChallenge => {
                    result.push(SlashedValidator::new(challenge.account_id.clone(), false));
                }
                Err(err) => return Err(err),
            }
        }
        Ok(result)
    }

    /// Verify header signature when the epoch is known, but not the whole chain.
    /// Same as verify_header_signature except it does not verify that block producer hasn't been slashed
    fn partial_verify_orphan_header_signature(&self, header: &BlockHeader) -> Result<bool, Error> {
        let block_producer =
            self.runtime_adapter.get_block_producer(header.epoch_id(), header.height())?;
        // DEVNOTE: we pass head which is not necessarily on block's chain, but it's only used for
        // slashing info which we will ignore
        let head = self.chain_store_update.head()?;
        let (block_producer, _slashed) = self.runtime_adapter.get_validator_by_account_id(
            header.epoch_id(),
            &head.last_block_hash,
            &block_producer,
        )?;
        Ok(header.signature().verify(header.hash().as_ref(), block_producer.public_key()))
    }
}

pub fn do_apply_chunks(
    work: Vec<Box<dyn FnOnce() -> Result<ApplyChunkResult, Error> + Send>>,
) -> Vec<Result<ApplyChunkResult, Error>> {
    work.into_par_iter().map(|task| task()).collect::<Vec<_>>()
}

pub fn collect_receipts<'a, T>(receipt_proofs: T) -> Vec<Receipt>
where
    T: IntoIterator<Item = &'a ReceiptProof>,
{
    receipt_proofs.into_iter().flat_map(|ReceiptProof(receipts, _)| receipts).cloned().collect()
}

pub fn collect_receipts_from_response(
    receipt_proof_response: &[ReceiptProofResponse],
) -> Vec<Receipt> {
    collect_receipts(
        receipt_proof_response.iter().flat_map(|ReceiptProofResponse(_, proofs)| proofs),
    )
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct ApplyStatePartsRequest {
    pub runtime: Arc<dyn RuntimeAdapter>,
    pub shard_id: ShardId,
    pub state_root: StateRoot,
    pub num_parts: u64,
    pub epoch_id: EpochId,
    pub sync_hash: CryptoHash,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct ApplyStatePartsResponse {
    pub apply_result: Result<(), near_chain_primitives::error::Error>,
    pub shard_id: ShardId,
    pub sync_hash: CryptoHash,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct BlockCatchUpRequest {
    pub sync_hash: CryptoHash,
    pub block_hash: CryptoHash,
    pub work: Vec<Box<dyn FnOnce() -> Result<ApplyChunkResult, Error> + Send>>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct BlockCatchUpResponse {
    pub sync_hash: CryptoHash,
    pub block_hash: CryptoHash,
    pub results: Vec<Result<ApplyChunkResult, Error>>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct StateSplitRequest {
    pub runtime: Arc<dyn RuntimeAdapter>,
    pub sync_hash: CryptoHash,
    pub shard_id: ShardId,
    pub shard_uid: ShardUId,
    pub state_root: StateRoot,
    pub next_epoch_shard_layout: ShardLayout,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct StateSplitResponse {
    pub sync_hash: CryptoHash,
    pub shard_id: ShardId,
    pub new_state_roots: Result<HashMap<ShardUId, StateRoot>, Error>,
}

/// Helper to track blocks catch up
/// Lifetime of a block_hash is as follows:
/// 1. It is added to pending blocks, either as first block of an epoch or because we (post)
///     processed previous block
/// 2. Block is preprocessed and scheduled for processing in sync jobs actor. Block hash
///     and state changes from preprocessing goes to scheduled blocks
/// 3. We've got response from sync jobs actor that block was processed. Block hash, state
///     changes from preprocessing and result of processing block are moved to processed blocks
/// 4. Results are postprocessed. If there is any error block goes back to pending to try again.
///     Otherwise results are commited, block is moved to done blocks and any blocks that
///     have this block as previous are added to pending
pub struct BlocksCatchUpState {
    /// Hash of first block of an epoch
    pub first_block_hash: CryptoHash,
    /// Epoch id
    pub epoch_id: EpochId,
    /// Collection of block hashes that are yet to be sent for processed
    pub pending_blocks: Vec<CryptoHash>,
    /// Map from block hashes that are scheduled for processing to saved store updates from their
    /// preprocessing
    pub scheduled_blocks: HashMap<CryptoHash, SavedStoreUpdate>,
    /// Map from block hashes that were processed to (saved store update, process results)
    pub processed_blocks:
        HashMap<CryptoHash, (SavedStoreUpdate, Vec<Result<ApplyChunkResult, Error>>)>,
    /// Collection of block hashes that are fully processed
    pub done_blocks: Vec<CryptoHash>,
}

impl BlocksCatchUpState {
    pub fn new(first_block_hash: CryptoHash, epoch_id: EpochId) -> Self {
        Self {
            first_block_hash,
            epoch_id,
            pending_blocks: vec![first_block_hash],
            scheduled_blocks: HashMap::new(),
            processed_blocks: HashMap::new(),
            done_blocks: vec![],
        }
    }

    pub fn is_finished(&self) -> bool {
        self.pending_blocks.is_empty()
            && self.scheduled_blocks.is_empty()
            && self.processed_blocks.is_empty()
    }
}
