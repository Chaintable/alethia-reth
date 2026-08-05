#![allow(missing_docs, clippy::missing_docs_in_private_items)]

use std::sync::Arc;

use alethia_reth_block::{config::TaikoEvmConfig, executor::is_zk_gas_difficulty_mismatch};
use alethia_reth_chainspec::{TAIKO_HOODI, spec::TaikoChainSpec};
use alethia_reth_evm::zk_gas::unzen::TX_INTRINSIC_ZK_GAS;
use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
use reth_ethereum_primitives::{Block, BlockBody, Receipt, TransactionSigned};
use reth_evm::{
    ConfigureEvm, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor},
};
use reth_primitives_traits::SealedBlock;
use reth_revm::{
    State,
    db::{
        InMemoryDB,
        states::{BundleState, bundle_state::BundleRetention},
    },
    state::{AccountInfo, Bytecode, bytecode::opcode},
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

const CHAIN_ID: u64 = 167_013;
const BLOCK_NUMBER: u64 = 11_981_108;
const UNZEN_TIMESTAMP: u64 = 1_781_787_600;
const CALLER: Address = Address::with_last_byte(0xc1);
const EMPTY_TARGET: Address = Address::with_last_byte(0xe1);
const CONTRACT: Address = Address::with_last_byte(0xf1);

struct ReplayObservation {
    committed_zk_gas: Vec<u64>,
    finalized_zk_gas: u64,
    execution: Result<BlockExecutionResult<Receipt>, BlockExecutionError>,
    bundle: BundleState,
    trace_nodes: Vec<usize>,
}

fn evm_config() -> TaikoEvmConfig {
    let chain_spec: Arc<TaikoChainSpec> = TAIKO_HOODI.clone();
    TaikoEvmConfig::new(chain_spec)
}

fn transaction(nonce: u64, to: Address, hash_byte: u8) -> Recovered<TransactionSigned> {
    let transaction = TxLegacy {
        chain_id: Some(ChainId::from(CHAIN_ID)),
        nonce,
        gas_price: 0,
        gas_limit: 100_000,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signature = Signature::new(U256::from(1), U256::from(2), false);
    Recovered::new_unchecked(
        Signed::new_unchecked(transaction, signature, B256::with_last_byte(hash_byte)).into(),
        CALLER,
    )
}

fn transactions() -> Vec<Recovered<TransactionSigned>> {
    vec![transaction(0, EMPTY_TARGET, 1), transaction(1, CONTRACT, 2)]
}

fn block(difficulty: U256) -> SealedBlock<Block> {
    let transactions =
        transactions().into_iter().map(|transaction| transaction.into_inner()).collect();
    SealedBlock::new_unhashed(Block {
        header: alloy_consensus::Header {
            number: BLOCK_NUMBER,
            difficulty,
            gas_limit: 30_000_000,
            timestamp: UNZEN_TIMESTAMP,
            base_fee_per_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            extra_data: Bytes::from(vec![0; 7]),
            ..Default::default()
        },
        body: BlockBody {
            transactions,
            withdrawals: Some(Withdrawals::default()),
            ..Default::default()
        },
    })
}

fn state_db() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        CALLER,
        AccountInfo { balance: U256::from(1_000_000_000_u64), ..Default::default() },
    );
    let code = Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        1,
        opcode::PUSH1,
        2,
        opcode::ADD,
        opcode::STOP,
    ]));
    db.insert_account_info(
        CONTRACT,
        AccountInfo {
            nonce: 1,
            code_hash: code.hash_slow(),
            code: Some(code),
            ..Default::default()
        },
    );
    db
}

fn finish_state(mut state: State<InMemoryDB>) -> BundleState {
    state.merge_transitions(BundleRetention::PlainState);
    state.take_bundle()
}

fn replay_without_tracing(difficulty: U256, validate_difficulty: bool) -> ReplayObservation {
    let config = evm_config();
    let block = block(difficulty);
    let mut state = State::builder().with_database(state_db()).with_bundle_update().build();
    let evm_env = config.evm_env(block.header()).expect("build Hoodi Unzen EVM environment");
    let mut ctx = config.context_for_block(&block).expect("build Hoodi Unzen block context");
    if !validate_difficulty {
        ctx.expected_difficulty = None;
    }
    let zk_gas_probe = ctx.clone();
    let evm = config.evm_with_env(&mut state, evm_env);
    let mut executor = config.create_executor(evm, ctx);

    executor.apply_pre_execution_changes().expect("apply Taiko pre-execution changes");
    let mut committed_zk_gas = Vec::new();
    for transaction in transactions() {
        executor.execute_transaction(transaction).expect("commit canonical transaction");
        committed_zk_gas.push(zk_gas_probe.finalized_block_zk_gas());
    }
    let execution = executor.finish().map(|(evm, execution)| {
        drop(evm);
        execution
    });
    let finalized_zk_gas = zk_gas_probe.finalized_block_zk_gas();
    let bundle = finish_state(state);

    ReplayObservation {
        committed_zk_gas,
        finalized_zk_gas,
        execution,
        bundle,
        trace_nodes: Vec::new(),
    }
}

fn replay_with_tracing(difficulty: U256) -> ReplayObservation {
    let config = evm_config();
    let block = block(difficulty);
    let mut state = State::builder().with_database(state_db()).with_bundle_update().build();
    let evm_env = config.evm_env(block.header()).expect("build Hoodi Unzen EVM environment");
    let ctx = config.context_for_block(&block).expect("build Hoodi Unzen block context");
    let zk_gas_probe = ctx.clone();
    let evm = config.evm_with_env_and_inspector(
        &mut state,
        evm_env,
        TracingInspector::new(TracingInspectorConfig::default_parity().set_steps(true)),
    );
    let mut executor = config.create_executor(evm, ctx);

    executor.evm_mut().set_inspector_enabled(false);
    executor.apply_pre_execution_changes().expect("apply Taiko pre-execution changes");
    executor.evm_mut().set_inspector_enabled(true);
    let mut committed_zk_gas = Vec::new();
    let mut trace_nodes = Vec::new();
    for transaction in transactions() {
        let output = executor
            .execute_transaction_without_commit(transaction)
            .expect("execute canonical transaction with tracing");
        trace_nodes.push(executor.evm().inspector().traces().nodes().len());
        executor.evm_mut().inspector_mut().fuse();
        executor.commit_transaction(output).expect("commit traced canonical transaction");
        committed_zk_gas.push(zk_gas_probe.finalized_block_zk_gas());
    }
    executor.evm_mut().set_inspector_enabled(false);
    let execution = executor.finish().map(|(evm, execution)| {
        drop(evm);
        execution
    });
    let finalized_zk_gas = zk_gas_probe.finalized_block_zk_gas();
    let bundle = finish_state(state);

    ReplayObservation { committed_zk_gas, finalized_zk_gas, execution, bundle, trace_nodes }
}

fn execution_result(observation: &ReplayObservation) -> &BlockExecutionResult<Receipt> {
    observation.execution.as_ref().expect("header difficulty must accept the replay")
}

fn assert_difficulty_mismatch(observation: &ReplayObservation, expected: U256, got: u64) {
    let error = observation.execution.as_ref().expect_err("mutated difficulty must be rejected");
    assert!(is_zk_gas_difficulty_mismatch(error));
    assert_eq!(
        error.to_string(),
        format!("zk gas header difficulty mismatch: expected {expected}, got {got}")
    );
}

#[test]
fn hoodi_unzen_tracing_matches_formal_executor_zk_gas_and_difficulty() {
    let reference = replay_without_tracing(U256::ZERO, false);
    let [first, second] = reference.committed_zk_gas.as_slice() else {
        panic!("the fixture must commit exactly two transactions");
    };
    assert_eq!(*first, TX_INTRINSIC_ZK_GAS);
    assert!(
        second - first > TX_INTRINSIC_ZK_GAS,
        "the contract transaction must commit intrinsic plus opcode zk gas"
    );
    assert_eq!(*second, reference.finalized_zk_gas);

    let header_difficulty = U256::from(reference.finalized_zk_gas);
    let non_tracing = replay_without_tracing(header_difficulty, true);
    let tracing = replay_with_tracing(header_difficulty);

    assert_eq!(non_tracing.committed_zk_gas, reference.committed_zk_gas);
    assert_eq!(tracing.committed_zk_gas, reference.committed_zk_gas);
    assert_eq!(non_tracing.finalized_zk_gas, reference.finalized_zk_gas);
    assert_eq!(tracing.finalized_zk_gas, reference.finalized_zk_gas);
    assert_eq!(U256::from(non_tracing.finalized_zk_gas), header_difficulty);
    assert_eq!(U256::from(tracing.finalized_zk_gas), header_difficulty);
    assert_eq!(tracing.trace_nodes.len(), reference.committed_zk_gas.len());
    assert!(tracing.trace_nodes.iter().all(|count| *count > 0));

    let non_tracing_execution = execution_result(&non_tracing);
    let tracing_execution = execution_result(&tracing);
    assert_eq!(tracing_execution.receipts, non_tracing_execution.receipts);
    assert_eq!(tracing_execution.gas_used, non_tracing_execution.gas_used);
    assert_eq!(tracing.bundle, non_tracing.bundle);

    let mutated_difficulty = header_difficulty + U256::from(1);
    let non_tracing_mutation = replay_without_tracing(mutated_difficulty, true);
    let tracing_mutation = replay_with_tracing(mutated_difficulty);
    assert_eq!(non_tracing_mutation.committed_zk_gas, reference.committed_zk_gas);
    assert_eq!(tracing_mutation.committed_zk_gas, reference.committed_zk_gas);
    assert_difficulty_mismatch(
        &non_tracing_mutation,
        mutated_difficulty,
        reference.finalized_zk_gas,
    );
    assert_difficulty_mismatch(&tracing_mutation, mutated_difficulty, reference.finalized_zk_gas);
}
