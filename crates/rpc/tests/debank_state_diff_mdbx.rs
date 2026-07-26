#![allow(missing_docs, clippy::missing_docs_in_private_items)]

use alethia_reth_rpc::debank::{
    build_storage_diff, decode_and_validate_storage_diff_exact, hashed_address, hashed_slot,
};
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use reth::{
    chainspec::ChainSpecBuilder,
    providers::test_utils::{create_test_provider_factory_with_chain_spec, insert_genesis},
};
use reth_revm::{
    db::{
        AccountStatus, BundleAccount, BundleState,
        states::{StorageSlot, StorageWithOriginalValues},
    },
    state::{AccountInfo, Bytecode},
};
use reth_trie::{HashedPostState, KeccakKeyHasher};
use std::{collections::BTreeMap, sync::Arc};

fn word(value: u64) -> B256 {
    B256::from(U256::from(value).to_be_bytes::<32>())
}

fn genesis_account(
    balance: u64,
    nonce: u64,
    code: Option<Bytes>,
    storage: &[(u64, u64)],
) -> GenesisAccount {
    GenesisAccount {
        nonce: Some(nonce),
        balance: U256::from(balance),
        code,
        storage: Some(storage.iter().map(|(slot, value)| (word(*slot), word(*value))).collect()),
        private_key: None,
    }
}

fn account_info(account: &GenesisAccount) -> AccountInfo {
    AccountInfo {
        balance: account.balance,
        nonce: account.nonce.unwrap_or_default(),
        code_hash: account.code.as_ref().map_or(KECCAK_EMPTY, keccak256),
        account_id: None,
        code: None,
    }
}

#[test]
fn decoded_state_diff_matches_replay_and_independent_final_mdbx_trie() {
    let ordinary = Address::repeat_byte(0x11);
    let cleared = Address::repeat_byte(0x22);
    let deleted = Address::repeat_byte(0x33);
    let recreated = Address::repeat_byte(0x44);

    let old_ordinary_code = Bytes::from_static(&[0x60, 0x00, 0x00]);
    let new_ordinary_code = Bytes::from_static(&[0x60, 0x01, 0x00]);
    let old_recreated_code = Bytes::from_static(&[0x60, 0x02, 0x00]);
    let new_recreated_code = Bytes::from_static(&[0x60, 0x03, 0x00]);

    let parent_ordinary = genesis_account(100, 1, Some(old_ordinary_code), &[(1, 7), (9, 90)]);
    let final_ordinary =
        genesis_account(250, 2, Some(new_ordinary_code.clone()), &[(1, 8), (9, 90)]);
    let parent_cleared = genesis_account(20, 0, None, &[(2, 5), (3, 6)]);
    let final_cleared = genesis_account(20, 0, None, &[(3, 6)]);
    let parent_deleted = genesis_account(30, 3, None, &[(4, 40), (5, 50)]);
    let parent_recreated = genesis_account(40, 4, Some(old_recreated_code), &[(6, 60), (7, 70)]);
    let final_recreated =
        genesis_account(400, 1, Some(new_recreated_code.clone()), &[(6, 600), (8, 80)]);

    let parent_alloc = BTreeMap::from([
        (ordinary, parent_ordinary.clone()),
        (cleared, parent_cleared.clone()),
        (deleted, parent_deleted.clone()),
        (recreated, parent_recreated.clone()),
    ]);
    let parent_spec = Arc::new(
        ChainSpecBuilder::mainnet()
            .genesis(Genesis { alloc: parent_alloc, ..Default::default() })
            .build(),
    );
    let parent_factory = create_test_provider_factory_with_chain_spec(parent_spec.clone());
    let parent_root =
        insert_genesis(&parent_factory, parent_spec).expect("install parent MDBX/trie state");
    let parent_state = parent_factory.latest().expect("open parent state provider");
    assert_eq!(
        parent_state.state_root(HashedPostState::default()).expect("read parent trie root"),
        parent_root
    );

    let final_alloc = BTreeMap::from([
        (ordinary, final_ordinary.clone()),
        (cleared, final_cleared.clone()),
        (recreated, final_recreated.clone()),
    ]);
    let final_spec = Arc::new(
        ChainSpecBuilder::mainnet()
            .genesis(Genesis { alloc: final_alloc, ..Default::default() })
            .build(),
    );
    let final_factory = create_test_provider_factory_with_chain_spec(final_spec.clone());
    let expected_final_root =
        insert_genesis(&final_factory, final_spec).expect("install independent final MDBX/trie");

    let mut bundle = BundleState::default();

    let mut ordinary_storage = StorageWithOriginalValues::default();
    ordinary_storage.insert(U256::from(1), StorageSlot::new_changed(U256::from(7), U256::from(8)));
    bundle.state.insert(
        ordinary,
        BundleAccount::new(
            Some(account_info(&parent_ordinary)),
            Some(account_info(&final_ordinary)),
            ordinary_storage,
            AccountStatus::Changed,
        ),
    );

    let mut cleared_storage = StorageWithOriginalValues::default();
    cleared_storage.insert(U256::from(2), StorageSlot::new_changed(U256::from(5), U256::ZERO));
    bundle.state.insert(
        cleared,
        BundleAccount::new(
            Some(account_info(&parent_cleared)),
            Some(account_info(&final_cleared)),
            cleared_storage,
            AccountStatus::Changed,
        ),
    );

    bundle.state.insert(
        deleted,
        BundleAccount::new(
            Some(account_info(&parent_deleted)),
            None,
            StorageWithOriginalValues::default(),
            AccountStatus::Destroyed,
        ),
    );

    let mut recreated_storage = StorageWithOriginalValues::default();
    recreated_storage
        .insert(U256::from(6), StorageSlot::new_changed(U256::from(60), U256::from(600)));
    recreated_storage.insert(U256::from(8), StorageSlot::new_changed(U256::ZERO, U256::from(80)));
    bundle.state.insert(
        recreated,
        BundleAccount::new(
            Some(account_info(&parent_recreated)),
            Some(account_info(&final_recreated)),
            recreated_storage,
            AccountStatus::DestroyedChanged,
        ),
    );

    for code in [new_ordinary_code, new_recreated_code] {
        bundle.contracts.insert(keccak256(&code), Bytecode::new_raw(code));
    }

    let replay_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
    let root_a = parent_state.state_root(replay_state).expect("compute replay root A");
    assert_eq!(root_a, expected_final_root);

    let diff = build_storage_diff(&bundle, root_a, parent_root).expect("build complete state diff");
    assert_eq!(diff.hash, root_a);
    assert_eq!(diff.parent_hash, parent_root);
    assert!(diff.deleted_accounts.contains(&hashed_address(deleted)));
    assert!(diff.deleted_accounts.contains(&hashed_address(recreated)));
    assert_eq!(diff.new_codes.len(), 2);

    let clear_diff = diff
        .storage_diffs
        .iter()
        .find(|storage| storage.address == hashed_address(cleared))
        .expect("slot clear must be emitted");
    assert_eq!(clear_diff.diffs.len(), 1);
    assert_eq!(clear_diff.diffs[0].index, hashed_slot(U256::from(2)));
    assert_eq!(clear_diff.diffs[0].value, U256::ZERO);

    let recreated_diff = diff
        .storage_diffs
        .iter()
        .find(|storage| storage.address == hashed_address(recreated))
        .expect("recreated account needs complete final storage");
    let mut expected_recreated_storage = vec![
        (hashed_slot(U256::from(6)), U256::from(600)),
        (hashed_slot(U256::from(8)), U256::from(80)),
    ];
    expected_recreated_storage.sort_unstable_by_key(|slot| slot.0);
    assert_eq!(
        recreated_diff.diffs.iter().map(|slot| (slot.index, slot.value)).collect::<Vec<_>>(),
        expected_recreated_storage
    );

    let encoded = alloy_rlp::encode(&diff);
    let decoded_state =
        decode_and_validate_storage_diff_exact(&encoded, &diff).expect("decode exact state diff");
    assert!(decoded_state.storages[&hashed_address(deleted)].wiped);
    assert!(decoded_state.storages[&hashed_address(recreated)].wiped);

    let root_b = parent_state.state_root(decoded_state).expect("compute decoded root B");
    assert_eq!(root_b, root_a);
    assert_eq!(root_b, expected_final_root);
}
