use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rayon::iter::{IntoParallelIterator, ParallelIterator};

use near_chain::chain::collect_receipts_from_response;
use near_chain::migrations::check_if_block_is_first_with_chunk_of_version;
use near_chain::types::ApplyTransactionResult;
use near_chain::{ChainStore, ChainStoreAccess, ChainStoreUpdate, RuntimeAdapter};
use near_chain_configs::Genesis;
use near_primitives::borsh::maybestd::sync::Arc;
use near_primitives::hash::CryptoHash;
use near_primitives::receipt::DelayedReceiptIndices;
use near_primitives::transaction::{ExecutionOutcomeWithId, ExecutionOutcomeWithIdAndProof};
use near_primitives::trie_key::TrieKey;
use near_primitives::types::chunk_extra::ChunkExtra;
use near_primitives::types::{BlockHeight, ShardId};
use near_store::{get, DBCol, Store};
use nearcore::NightshadeRuntime;

fn inc_and_report_progress(cnt: &AtomicU64) {
    let prev = cnt.fetch_add(1, Ordering::Relaxed);
    if (prev + 1) % 10000 == 0 {
        println!("Processed {} blocks", prev + 1);
    }
}

fn old_outcomes(
    store: Arc<Store>,
    new_outcomes: &Vec<ExecutionOutcomeWithId>,
) -> Vec<ExecutionOutcomeWithId> {
    new_outcomes
        .iter()
        .map(|outcome| {
            store
                .get_ser::<Vec<ExecutionOutcomeWithIdAndProof>>(
                    DBCol::ColTransactionResult,
                    outcome.id.as_ref(),
                )
                .unwrap()
                .unwrap()[0]
                .outcome_with_id
                .clone()
        })
        .collect()
}

fn maybe_add_to_csv(csv_file_mutex: &Mutex<Option<&mut File>>, s: &str) {
    let mut csv_file = csv_file_mutex.lock().unwrap();
    if let Some(csv_file) = csv_file.as_mut() {
        write!(csv_file, "{}\n", s).unwrap();
    }
}

pub fn apply_chain_range(
    store: Arc<Store>,
    genesis: &Genesis,
    start_height: Option<BlockHeight>,
    end_height: Option<BlockHeight>,
    shard_id: ShardId,
    runtime: NightshadeRuntime,
    verbose_output: bool,
    csv_file: Option<&mut File>,
) {
    let runtime_adapter: Arc<dyn RuntimeAdapter> = Arc::new(runtime);
    let chain_store = ChainStore::new(store.clone(), genesis.config.genesis_height);
    let end_height = end_height.unwrap_or_else(|| chain_store.head().unwrap().height);
    let start_height = start_height.unwrap_or_else(|| chain_store.tail().unwrap());

    println!(
        "Applying chunks in the range {}..={} for shard_id {}",
        start_height, end_height, shard_id
    );

    println!("Printing results including outcomes of applying receipts");
    let csv_file_mutex = Arc::new(Mutex::new(csv_file));
    maybe_add_to_csv(&csv_file_mutex, "Height,Hash,Author,#Tx,#Receipt,Timestamp,GasUsed,ChunkPresent,#ProcessedDelayedReceipts,#DelayedReceipts");

    let processed_blocks_cnt = AtomicU64::new(0);
    (start_height..=end_height).into_iter().for_each(|height| {
            let mut chain_store = ChainStore::new(store.clone(), genesis.config.genesis_height);
            let block_hash = match chain_store.get_block_hash_by_height(height) {
                Ok(block_hash) => block_hash,
                Err(_) => {
                    // Skipping block because it's not available in ChainStore.
                    inc_and_report_progress(&processed_blocks_cnt);
                    return;
                },
            };
            let block = chain_store.get_block(&block_hash).unwrap().clone();
            let shard_uid =
                runtime_adapter.shard_id_to_uid(shard_id, block.header().epoch_id()).unwrap();
            assert!(block.chunks().len() > 0);
            let mut existing_chunk_extra = None;
            let mut prev_chunk_extra = None;
            let mut num_tx = 0;
            let mut num_receipt = 0;
            let chunk_present: bool;

            let block_author = runtime_adapter.get_block_producer(&block.header().epoch_id(), block.header().height()).unwrap();

            let apply_result = if *block.header().prev_hash() == CryptoHash::default() {
                if verbose_output {
                    println!("Skipping the genesis block #{}.", height);
                }
                inc_and_report_progress(&processed_blocks_cnt);
                return;
            } else if block.chunks()[shard_id as usize].height_included() == height {
                chunk_present = true;
                let res_existing_chunk_extra = chain_store.get_chunk_extra(&block_hash, &shard_uid);
                assert!(res_existing_chunk_extra.is_ok(), "Can't get existing chunk extra for block #{}", height);
                existing_chunk_extra = Some(res_existing_chunk_extra.unwrap().clone());
                let chunk = chain_store
                    .get_chunk(&block.chunks()[shard_id as usize].chunk_hash())
                    .unwrap()
                    .clone();

                let prev_block = match chain_store.get_block(&block.header().prev_hash()) {
                    Ok(prev_block) => prev_block.clone(),
                    Err(_) => {
                        if verbose_output {
                            println!("Skipping applying block #{} because the previous block is unavailable and I can't determine the gas_price to use.", height);
                        }
                        maybe_add_to_csv(&csv_file_mutex, &format!("{},{},{},,,{},,{},,", height, block_hash, block_author, block.header().raw_timestamp(), chunk_present));
                        inc_and_report_progress(&processed_blocks_cnt);
                        return;
                    },
                };

                let mut chain_store_update = ChainStoreUpdate::new(&mut chain_store);
                let receipt_proof_response = chain_store_update
                    .get_incoming_receipts_for_shard(
                        shard_id,
                        block_hash,
                        prev_block.chunks()[shard_id as usize].height_included(),
                    )
                    .unwrap();
                let receipts = collect_receipts_from_response(&receipt_proof_response);

                let chunk_inner = chunk.cloned_header().take_inner();
                let is_first_block_with_chunk_of_version =
                    check_if_block_is_first_with_chunk_of_version(
                        &mut chain_store,
                        runtime_adapter.as_ref(),
                        block.header().prev_hash(),
                        shard_id,
                    )
                        .unwrap();

                num_receipt = receipts.len();
                num_tx = chunk.transactions().len();
                runtime_adapter
                    .apply_transactions(
                        shard_id,
                        chunk_inner.prev_state_root(),
                        height,
                        block.header().raw_timestamp(),
                        block.header().prev_hash(),
                        block.hash(),
                        &receipts,
                        chunk.transactions(),
                        chunk_inner.validator_proposals(),
                        prev_block.header().gas_price(),
                        chunk_inner.gas_limit(),
                        &block.header().challenges_result(),
                        *block.header().random_value(),
                        true,
                        is_first_block_with_chunk_of_version,
                        None,
                    )
                    .unwrap()
            } else {
                chunk_present = false;
                let chunk_extra = chain_store.get_chunk_extra(block.header().prev_hash(), &shard_uid).unwrap().clone();
                prev_chunk_extra = Some(chunk_extra.clone());

                runtime_adapter
                    .apply_transactions(
                        shard_id,
                        chunk_extra.state_root(),
                        block.header().height(),
                        block.header().raw_timestamp(),
                        block.header().prev_hash(),
                        &block.hash(),
                        &[],
                        &[],
                        chunk_extra.validator_proposals(),
                        block.header().gas_price(),
                        chunk_extra.gas_limit(),
                        &block.header().challenges_result(),
                        *block.header().random_value(),
                        false,
                        false,
                        None,
                    )
                    .unwrap()
            };

            let (outcome_root, _) =
                ApplyTransactionResult::compute_outcomes_proof(&apply_result.outcomes);
            let chunk_extra = ChunkExtra::new(
                &apply_result.new_root,
                outcome_root,
                apply_result.validator_proposals,
                apply_result.total_gas_burnt,
                genesis.config.gas_limit,
                apply_result.total_balance_burnt,
            );

            // debug comment. failing with StorageInconsistentState("Trie node missing")
            // let state_update = runtime_adapter.get_tries().new_trie_update(shard_uid, *chunk_extra.state_root());
            let delayed_indices: Option<DelayedReceiptIndices> = None; // get::<DelayedReceiptIndices>(&state_update, &TrieKey::DelayedReceiptIndices).unwrap();

        match existing_chunk_extra {
            Some(existing_chunk_extra) => {
                if verbose_output {
                    println!("block_height: {}, block_hash: {}\nchunk_extra: {:#?}\nexisting_chunk_extra: {:#?}\noutcomes: {:#?}", height, block_hash, chunk_extra, existing_chunk_extra, apply_result.outcomes);
                }
                assert_eq!(existing_chunk_extra, chunk_extra, "Got a different ChunkExtra:\nblock_height: {}, block_hash: {}\nchunk_extra: {:#?}\nexisting_chunk_extra: {:#?}\nnew outcomes: {:#?}\n\nold outcomes: {:#?}\n", height, block_hash, chunk_extra, existing_chunk_extra, apply_result.outcomes, old_outcomes(store.clone(), &apply_result.outcomes));
            },
            None => {
                assert!(prev_chunk_extra.is_some());
                assert!(apply_result.outcomes.is_empty());
                if verbose_output {
                    println!("block_height: {}, block_hash: {}\nchunk_extra: {:#?}\nprev_chunk_extra: {:#?}\noutcomes: {:#?}", height, block_hash, chunk_extra, prev_chunk_extra, apply_result.outcomes);
                }
            },
        };
        maybe_add_to_csv(&csv_file_mutex, &format!("{},{},{},{},{},{},{},{},{},{}", height, block_hash, block_author, num_tx, num_receipt, block.header().raw_timestamp(), apply_result.total_gas_burnt, chunk_present, apply_result.processed_delayed_receipts.len(), delayed_indices.map_or(0,|d|d.next_available_index-d.first_index)));
        inc_and_report_progress(&processed_blocks_cnt);
    });

    println!(
        "No differences found after applying chunks in the range {}..={} for shard_id {}",
        start_height, end_height, shard_id
    );
}

pub(crate) fn apply_chain_range_all_shards(
    store: Arc<Store>,
    genesis: &Genesis,
    start_height: Option<BlockHeight>,
    end_height: Option<BlockHeight>,
    runtime: NightshadeRuntime,
    verbose_output: bool,
    csv_file: Option<&mut File>,
) {
    let csv_file_mutex = Arc::new(Mutex::new(csv_file));
    let runtime_adapter: Arc<dyn RuntimeAdapter> = Arc::new(runtime);
    let chain_store = ChainStore::new(store.clone(), genesis.config.genesis_height);
    let end_height = end_height.unwrap_or_else(|| chain_store.head().unwrap().height);
    let start_height = start_height.unwrap_or_else(|| chain_store.tail().unwrap());
    let mut chain_store = ChainStore::new(store.clone(), genesis.config.genesis_height);

    (start_height..=end_height).into_iter().for_each(|height| {
        let mut num_tx = 0;
        let mut num_receipt = 0;
        let chunk_present: bool;

        let block_hash = match chain_store.get_block_hash_by_height(height) {
            Ok(block_hash) => block_hash,
            Err(_) => {
                // Skipping block because it's not available in ChainStore.
                inc_and_report_progress(&processed_blocks_cnt);
                return;
            }
        };
        let block = chain_store.get_block(&block_hash).unwrap().clone();
        (0..4).into_par_iter().for_each(|shard_id| {
            let shard_uid = runtime_adapter.shard_id_to_uid(shard_id, block.header().epoch_id()).unwrap();

            let block_author = runtime_adapter
                .get_block_producer(&block.header().epoch_id(), block.header().height())
                .unwrap();

            if *block.header().prev_hash() == CryptoHash::default() {
                if verbose_output {
                    println!("Skipping the genesis block #{}.", height);
                }
                inc_and_report_progress(&processed_blocks_cnt);
                return;
            } else if block.chunks()[shard_id as usize].height_included() == height {
                chunk_present = true;
                let res_existing_chunk_extra = chain_store.get_chunk_extra(&block_hash, &shard_uid);
                assert!(
                    res_existing_chunk_extra.is_ok(),
                    "Can't get existing chunk extra for block #{}",
                    height
                );
                existing_chunk_extra = Some(res_existing_chunk_extra.unwrap().clone());
                let chunk =
                    chain_store.get_chunk(&block.chunks()[shard_id as usize].chunk_hash()).unwrap().clone();

                let prev_block = match chain_store.get_block(&block.header().prev_hash()) {
                    Ok(prev_block) => prev_block.clone(),
                    Err(_) => {
                        if verbose_output {
                            println!("Skipping applying block #{} because the previous block is unavailable and I can't determine the gas_price to use.", height);
                        }
                        maybe_add_to_csv(
                            &csv_file_mutex,
                            &format!(
                                "{},{},{},,,{},,{},,",
                                height,
                                block_hash,
                                block_author,
                                block.header().raw_timestamp(),
                                chunk_present
                            ),
                        );
                        inc_and_report_progress(&processed_blocks_cnt);
                        return;
                    }
                };

                let mut chain_store_update = ChainStoreUpdate::new(&mut chain_store);
                let receipt_proof_response = chain_store_update
                    .get_incoming_receipts_for_shard(
                        shard_id,
                        block_hash,
                        prev_block.chunks()[shard_id as usize].height_included(),
                    )
                    .unwrap();
                let receipts = collect_receipts_from_response(&receipt_proof_response);

                let chunk_inner = chunk.cloned_header().take_inner();
                let is_first_block_with_chunk_of_version = check_if_block_is_first_with_chunk_of_version(
                    &mut chain_store,
                    runtime_adapter.as_ref(),
                    block.header().prev_hash(),
                    shard_id,
                )
                    .unwrap();

                num_receipt = receipts.len();
                num_tx = chunk.transactions().len();
                runtime_adapter
                    .apply_transactions(
                        shard_id,
                        chunk_inner.prev_state_root(),
                        height,
                        block.header().raw_timestamp(),
                        block.header().prev_hash(),
                        block.hash(),
                        &receipts,
                        chunk.transactions(),
                        chunk_inner.validator_proposals(),
                        prev_block.header().gas_price(),
                        chunk_inner.gas_limit(),
                        &block.header().challenges_result(),
                        *block.header().random_value(),
                        true,
                        is_first_block_with_chunk_of_version,
                        None,
                    )
                    .unwrap()
            } else {
                chunk_present = false;
                let chunk_extra =
                    chain_store.get_chunk_extra(block.header().prev_hash(), &shard_uid).unwrap().clone();
                prev_chunk_extra = Some(chunk_extra.clone());

                runtime_adapter
                    .apply_transactions(
                        shard_id,
                        chunk_extra.state_root(),
                        block.header().height(),
                        block.header().raw_timestamp(),
                        block.header().prev_hash(),
                        &block.hash(),
                        &[],
                        &[],
                        chunk_extra.validator_proposals(),
                        block.header().gas_price(),
                        chunk_extra.gas_limit(),
                        &block.header().challenges_result(),
                        *block.header().random_value(),
                        false,
                        false,
                        None,
                    )
                    .unwrap()
            };
        });
    });
}

#[cfg(test)]
mod test {
    use std::io::{Read, Seek, SeekFrom};
    use std::path::Path;
    use std::sync::Arc;

    use near_chain::{ChainGenesis, Provenance};
    use near_chain_configs::Genesis;
    use near_client::test_utils::TestEnv;
    use near_crypto::{InMemorySigner, KeyType};
    use near_primitives::transaction::SignedTransaction;
    use near_primitives::types::{BlockHeight, BlockHeightDelta, NumBlocks};
    use near_store::test_utils::create_test_store;
    use near_store::Store;
    use nearcore::config::GenesisExt;
    use nearcore::config::TESTING_INIT_STAKE;
    use nearcore::NightshadeRuntime;

    use crate::apply_chain_range::apply_chain_range;

    fn setup(epoch_length: NumBlocks) -> (Arc<Store>, Genesis, TestEnv) {
        let mut genesis =
            Genesis::test(vec!["test0".parse().unwrap(), "test1".parse().unwrap()], 1);
        genesis.config.num_block_producer_seats = 2;
        genesis.config.num_block_producer_seats_per_shard = vec![2];
        genesis.config.epoch_length = epoch_length;
        let store = create_test_store();
        let nightshade_runtime = NightshadeRuntime::test(Path::new("."), store.clone(), &genesis);
        let mut chain_genesis = ChainGenesis::test();
        chain_genesis.epoch_length = epoch_length;
        chain_genesis.gas_limit = genesis.config.gas_limit;
        let env = TestEnv::builder(chain_genesis)
            .validator_seats(2)
            .runtime_adapters(vec![Arc::new(nightshade_runtime)])
            .build();
        (store, genesis, env)
    }

    /// Produces blocks, avoiding the potential failure where the client is not the
    /// block producer for each subsequent height (this can happen when a new validator
    /// is staked since they will also have heights where they should produce the block instead).
    fn safe_produce_blocks(
        env: &mut TestEnv,
        initial_height: BlockHeight,
        num_blocks: BlockHeightDelta,
        block_without_chunks: Option<BlockHeight>,
    ) {
        let mut h = initial_height;
        let mut blocks = vec![];
        for _ in 1..=num_blocks {
            let mut block = None;
            // `env.clients[0]` may not be the block producer at `h`,
            // loop until we find a height env.clients[0] should produce.
            while block.is_none() {
                block = env.clients[0].produce_block(h).unwrap();
                h += 1;
            }
            let mut block = block.unwrap();
            if let Some(block_without_chunks) = block_without_chunks {
                if block_without_chunks == h {
                    assert!(!blocks.is_empty());
                    testlib::process_blocks::set_no_chunk_in_block(
                        &mut block,
                        &blocks.last().unwrap(),
                    )
                }
            }
            blocks.push(block.clone());
            env.process_block(0, block, Provenance::PRODUCED);
        }
    }

    #[test]
    fn test_apply_chain_range() {
        let epoch_length = 4;
        let (store, genesis, mut env) = setup(epoch_length);
        let genesis_hash = *env.clients[0].chain.genesis().hash();
        let signer = InMemorySigner::from_seed("test1".parse().unwrap(), KeyType::ED25519, "test1");
        let tx = SignedTransaction::stake(
            1,
            "test1".parse().unwrap(),
            &signer,
            TESTING_INIT_STAKE,
            signer.public_key.clone(),
            genesis_hash,
        );
        env.clients[0].process_tx(tx, false, false);

        safe_produce_blocks(&mut env, 1, epoch_length * 2 + 1, None);

        let runtime = NightshadeRuntime::test(Path::new("."), store.clone(), &genesis);
        apply_chain_range(store, &genesis, None, None, 0, runtime, true, None);
    }

    #[test]
    fn test_apply_chain_range_no_chunks() {
        let epoch_length = 4;
        let (store, genesis, mut env) = setup(epoch_length);
        let genesis_hash = *env.clients[0].chain.genesis().hash();
        let signer = InMemorySigner::from_seed("test1".parse().unwrap(), KeyType::ED25519, "test1");
        let tx = SignedTransaction::stake(
            1,
            "test1".parse().unwrap(),
            &signer,
            TESTING_INIT_STAKE,
            signer.public_key.clone(),
            genesis_hash,
        );
        env.clients[0].process_tx(tx, false, false);

        safe_produce_blocks(&mut env, 1, epoch_length * 2 + 1, Some(5));

        let runtime = NightshadeRuntime::test(Path::new("."), store.clone(), &genesis);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        apply_chain_range(store, &genesis, None, None, 0, runtime, true, Some(file.as_file_mut()));
        let mut csv = String::new();
        file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
        file.as_file_mut().read_to_string(&mut csv).unwrap();
        let lines: Vec<&str> = csv.split("\n").collect();
        assert!(lines[0].contains("Height"));
        let mut has_tx = 0;
        let mut no_tx = 0;
        for line in &lines {
            if line.contains(",test0,1,0,") {
                has_tx += 1;
            }
            if line.contains(",test0,0,0,") {
                no_tx += 1;
            }
        }
        assert_eq!(has_tx, 1, "{:#?}", lines);
        assert_eq!(no_tx, 8, "{:#?}", lines);
    }
}
