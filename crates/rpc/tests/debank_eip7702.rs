#![allow(missing_docs, clippy::missing_docs_in_private_items)]

use std::{borrow::Cow, sync::Arc};

use alethia_reth_block::{
    config::TaikoEvmConfig, executor::TaikoBlockExecutor, factory::TaikoBlockExecutionCtx,
};
use alethia_reth_chainspec::{TAIKO_MAINNET, hardfork::TaikoHardfork, spec::TaikoChainSpec};
use alethia_reth_rpc::debank::{
    BlockStorageDiff, build_storage_diff, decode_and_validate_storage_diff_exact, hashed_address,
    hashed_slot,
};
use alloy_consensus::{Header, Signed, TxEip7702, constants::KECCAK_EMPTY, transaction::Recovered};
use alloy_eips::{eip2930::AccessList, eip4895::Withdrawals, eip7702::Authorization};
use alloy_hardforks::ForkCondition;
use alloy_primitives::{Address, B256, Bytes, Signature, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    ConfigureEvm,
    block::{BlockExecutionError, BlockExecutor, BlockValidationError, TxResult},
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_revm::{
    State,
    db::{
        InMemoryDB,
        states::{BundleState, bundle_state::BundleRetention},
    },
    state::{AccountInfo, Bytecode, bytecode::opcode},
};
use reth_trie::{HashedPostState, KeccakKeyHasher};

const CHAIN_ID: u64 = 167_000;
const BLOCK_NUMBER: u64 = 5_000_001;
const UNZEN_TIMESTAMP: u64 = 100;
const CALLER: Address = Address::with_last_byte(0xc1);
const DELEGATE: Address = Address::with_last_byte(0xd1);
const AUTHORITY_BALANCE: u64 = 7;
const STORAGE_KEY: u64 = 1;
const STORAGE_VALUE: u64 = 42;

struct Replay {
    authority: Address,
    bundle: BundleState,
    error: Option<BlockExecutionError>,
}

fn chain_spec() -> Arc<TaikoChainSpec> {
    let mut spec = (*TAIKO_MAINNET).as_ref().clone();
    spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
    spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(UNZEN_TIMESTAMP));
    Arc::new(spec)
}

fn authority_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::with_last_byte(1))
        .expect("fixed non-zero private key must be valid")
}

fn delegate_code() -> Bytecode {
    Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        STORAGE_VALUE as u8,
        opcode::PUSH1,
        STORAGE_KEY as u8,
        opcode::SSTORE,
        opcode::STOP,
    ]))
}

fn state_db(authority: Address) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        CALLER,
        AccountInfo { balance: U256::from(10_000_000_000_u64), ..Default::default() },
    );
    db.insert_account_info(
        authority,
        AccountInfo { balance: U256::from(AUTHORITY_BALANCE), ..Default::default() },
    );
    let code = delegate_code();
    db.insert_account_info(
        DELEGATE,
        AccountInfo {
            nonce: 1,
            code_hash: code.hash_slow(),
            code: Some(code),
            ..Default::default()
        },
    );
    db
}

fn recovered_type4(authority_nonce: u64) -> (Address, Recovered<TransactionSigned>) {
    let signer = authority_signer();
    let authority = signer.address();
    let authorization =
        Authorization { chain_id: U256::from(CHAIN_ID), address: DELEGATE, nonce: authority_nonce };
    let signature = signer
        .sign_hash_sync(&authorization.signature_hash())
        .expect("fixed signer must sign authorization");
    let authorization = authorization.into_signed(signature);
    assert_eq!(
        authorization.recover_authority().expect("authorization signature must recover"),
        authority
    );

    let transaction = TxEip7702 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: 500_000,
        max_fee_per_gas: 0,
        max_priority_fee_per_gas: 0,
        to: authority,
        value: U256::ZERO,
        access_list: AccessList::default(),
        authorization_list: vec![authorization],
        input: Bytes::new(),
    };
    let outer_signature = Signature::new(U256::from(1), U256::from(2), false);
    let transaction = Signed::new_unchecked(
        transaction,
        outer_signature,
        B256::with_last_byte(authority_nonce as u8 + 1),
    )
    .into();
    (authority, Recovered::new_unchecked(transaction, CALLER))
}

fn replay(timestamp: u64, authority_nonce: u64) -> Replay {
    let spec = chain_spec();
    let (authority, transaction) = recovered_type4(authority_nonce);
    let mut state =
        State::builder().with_database(state_db(authority)).with_bundle_update().build();
    let is_unzen_active = timestamp >= UNZEN_TIMESTAMP;
    let header = Header {
        number: BLOCK_NUMBER,
        gas_limit: 30_000_000,
        timestamp,
        extra_data: Bytes::from(vec![0; 7]),
        base_fee_per_gas: Some(0),
        parent_beacon_block_root: is_unzen_active.then_some(B256::ZERO),
        ..Default::default()
    };
    let evm_config = TaikoEvmConfig::new(spec.clone());
    let evm =
        evm_config.evm_for_block(&mut state, &header).expect("construct fork-correct Taiko EVM");
    let ctx = TaikoBlockExecutionCtx {
        parent_hash: B256::ZERO,
        parent_beacon_block_root: is_unzen_active.then_some(B256::ZERO),
        ommers: &[],
        withdrawals: Some(Cow::Owned(Withdrawals::default())),
        basefee_per_gas: 0,
        extra_data: header.extra_data,
        is_unzen_active,
        expected_difficulty: None,
        finalized_block_zk_gas: Default::default(),
    };
    let mut executor = TaikoBlockExecutor::new(evm, ctx, spec, RethReceiptBuilder::default());
    executor.apply_pre_execution_changes().expect("apply Taiko pre-execution changes");

    let error = match executor.execute_transaction_without_commit(transaction) {
        Ok(output) => {
            assert!(output.result().result.is_success(), "type-4 transaction must succeed");
            executor.commit_transaction(output).expect("commit type-4 transaction");
            let (evm, result) = executor.finish().expect("finish Taiko execution");
            assert_eq!(result.receipts.len(), 1);
            drop(evm);
            None
        }
        Err(error) => {
            drop(executor);
            Some(error)
        }
    };

    state.merge_transitions(BundleRetention::PlainState);
    Replay { authority, bundle: state.take_bundle(), error }
}

fn assert_exact_state_diff(bundle: &BundleState) -> BlockStorageDiff {
    let diff = build_storage_diff(bundle, B256::repeat_byte(0x22), B256::repeat_byte(0x11))
        .expect("build state diff from replay bundle");
    let wire = alloy_rlp::encode(&diff);
    let decoded = decode_and_validate_storage_diff_exact(&wire, &diff)
        .expect("decode exact state diff without trailing bytes or semantic drift");
    let mut replay_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
    // `Account::from(AccountInfo)` represents empty bytecode as `None`, while the state-diff
    // decoder preserves the explicitly encoded KECCAK_EMPTY hash. Both encode to the same trie
    // account; normalize only this representation detail before comparing the semantic maps.
    for account in replay_state.accounts.values_mut().flatten() {
        account.bytecode_hash.get_or_insert(KECCAK_EMPTY);
    }
    assert_eq!(decoded, replay_state);
    diff
}

#[test]
fn pre_unzen_rejects_type4_without_committing_transaction_state() {
    let replay = replay(UNZEN_TIMESTAMP - 1, 0);
    let error = replay.error.expect("pre-Unzen type-4 transaction must be rejected");
    let validation = error.as_validation().expect("rejection must be a validation error");
    let BlockValidationError::InvalidTx { error, .. } = validation else {
        panic!("unexpected pre-Unzen error: {validation}");
    };
    assert_eq!(error.to_string(), "Eip7702 is not supported");

    let diff = assert_exact_state_diff(&replay.bundle);
    let authority = hashed_address(replay.authority);
    let caller = hashed_address(CALLER);
    assert!(!diff.new_accounts.iter().any(|account| account.address == authority));
    assert!(!diff.new_accounts.iter().any(|account| account.address == caller));
    assert!(!diff.storage_diffs.iter().any(|storage| storage.address == authority));
    let delegation_hash = Bytecode::new_eip7702(DELEGATE).hash_slow();
    assert!(!diff.new_codes.iter().any(|code| code.code_hash == delegation_hash));
}

#[test]
fn unzen_executes_type4_and_roundtrips_complete_state_diff() {
    let replay = replay(UNZEN_TIMESTAMP, 0);
    assert!(replay.error.is_none(), "Unzen type-4 transaction must execute");

    let expected_delegation = Bytecode::new_eip7702(DELEGATE);
    let expected_code = expected_delegation.original_bytes();
    let expected_code_hash = expected_delegation.hash_slow();
    let account = replay
        .bundle
        .account(&replay.authority)
        .expect("authority must be present in committed bundle");
    let info = account.account_info().expect("authority account must exist");
    assert_eq!(info.balance, U256::from(AUTHORITY_BALANCE));
    assert_eq!(info.nonce, 1);
    assert_eq!(info.code_hash, expected_code_hash);
    assert_eq!(
        info.code.expect("authority must carry delegation bytecode").original_bytes(),
        expected_code
    );
    assert_eq!(account.storage_slot(U256::from(STORAGE_KEY)), Some(U256::from(STORAGE_VALUE)));

    let diff = assert_exact_state_diff(&replay.bundle);
    let authority = hashed_address(replay.authority);
    let account = diff
        .new_accounts
        .iter()
        .find(|account| account.address == authority)
        .expect("state diff must contain delegated authority");
    assert_eq!(account.balance, U256::from(AUTHORITY_BALANCE));
    assert_eq!(account.nonce, 1);
    assert_eq!(account.code_hash, expected_code_hash);
    let storage = diff
        .storage_diffs
        .iter()
        .find(|storage| storage.address == authority)
        .expect("state diff must contain delegated authority storage");
    assert_eq!(storage.diffs.len(), 1);
    assert_eq!(storage.diffs[0].index, hashed_slot(U256::from(STORAGE_KEY)));
    assert_eq!(storage.diffs[0].value, U256::from(STORAGE_VALUE));
    let code = diff
        .new_codes
        .iter()
        .find(|code| code.code_hash == expected_code_hash)
        .expect("state diff must contain delegation bytecode");
    assert_eq!(code.code, expected_code);
}

#[test]
fn unzen_stale_authorization_does_not_emit_fake_delegation_state() {
    let replay = replay(UNZEN_TIMESTAMP, 1);
    assert!(replay.error.is_none(), "stale authorization is ignored by EIP-7702");

    let diff = assert_exact_state_diff(&replay.bundle);
    let authority = hashed_address(replay.authority);
    assert!(!diff.new_accounts.iter().any(|account| account.address == authority));
    assert!(!diff.storage_diffs.iter().any(|storage| storage.address == authority));
    let delegation_hash = Bytecode::new_eip7702(DELEGATE).hash_slow();
    assert!(!diff.new_codes.iter().any(|code| code.code_hash == delegation_hash));

    let caller = diff
        .new_accounts
        .iter()
        .find(|account| account.address == hashed_address(CALLER))
        .expect("outer transaction must still commit its caller nonce");
    assert_eq!(caller.nonce, 1);
}
