use crate::routing::routing_table_view::RoutingTableView;
use crate::test_utils::{random_epoch_id, random_peer_id};
use near_crypto::Signature;
use near_primitives::network::AnnounceAccount;
use near_store::test_utils::create_test_store;

#[test]
fn announcement_same_epoch() {
    let store = create_test_store();

    let peer_id0 = random_peer_id();
    let peer_id1 = random_peer_id();
    let epoch_id0 = random_epoch_id();

    let mut routing_table = RoutingTableView::new(store);

    let announce0 = AnnounceAccount {
        account_id: "near0".parse().unwrap(),
        peer_id: peer_id0.clone(),
        epoch_id: epoch_id0.clone(),
        signature: Signature::default(),
    };

    // Same as announce1 but with different peer id
    let announce1 = AnnounceAccount {
        account_id: "near0".parse().unwrap(),
        peer_id: peer_id1.clone(),
        epoch_id: epoch_id0,
        signature: Signature::default(),
    };

    routing_table.add_account(announce0.clone());
    assert!(routing_table.contains_account(&announce0));
    assert!(routing_table.contains_account(&announce1));
    assert_eq!(routing_table.get_announce_accounts().count(), 1);
    assert_eq!(routing_table.account_owner(&announce0.account_id).unwrap(), peer_id0);
    routing_table.add_account(announce1.clone());
    assert_eq!(routing_table.get_announce_accounts().count(), 1);
    assert_eq!(routing_table.account_owner(&announce1.account_id).unwrap(), peer_id1);
}

#[test]
fn dont_load_on_build() {
    let store = create_test_store();

    let peer_id0 = random_peer_id();
    let peer_id1 = random_peer_id();
    let epoch_id0 = random_epoch_id();
    let epoch_id1 = random_epoch_id();

    let mut routing_table = RoutingTableView::new(store.clone());

    let announce0 = AnnounceAccount {
        account_id: "near0".parse().unwrap(),
        peer_id: peer_id0.clone(),
        epoch_id: epoch_id0,
        signature: Signature::default(),
    };

    // Same as announce1 but with different peer id
    let announce1 = AnnounceAccount {
        account_id: "near1".parse().unwrap(),
        peer_id: peer_id1,
        epoch_id: epoch_id1,
        signature: Signature::default(),
    };

    routing_table.add_account(announce0.clone());
    routing_table.add_account(announce1.clone());
    let accounts: Vec<&AnnounceAccount> = routing_table.get_announce_accounts().collect();
    assert!(vec![announce0, announce1].iter().all(|announce| { accounts.contains(&announce) }));
    assert_eq!(accounts.len(), 2);

    let routing_table1 = RoutingTableView::new(store);
    assert_eq!(routing_table1.get_announce_accounts().count(), 0);
}

#[test]
fn load_from_disk() {
    let store = create_test_store();

    let peer_id0 = random_peer_id();
    let epoch_id0 = random_epoch_id();

    let mut routing_table = RoutingTableView::new(store.clone());
    let mut routing_table1 = RoutingTableView::new(store);

    let announce0 = AnnounceAccount {
        account_id: "near0".parse().unwrap(),
        peer_id: peer_id0.clone(),
        epoch_id: epoch_id0,
        signature: Signature::default(),
    };

    // Announcement is added to cache of the first routing table and to disk
    routing_table.add_account(announce0.clone());
    assert_eq!(routing_table.get_announce_accounts().count(), 1);
    // Cache of second routing table is empty
    assert_eq!(routing_table1.get_announce_accounts().count(), 0);
    // Try to find this peer and load it from disk
    assert_eq!(routing_table1.account_owner(&announce0.account_id).unwrap(), peer_id0);
    // Cache of second routing table should contain account loaded from disk
    assert_eq!(routing_table1.get_announce_accounts().count(), 1);
}
