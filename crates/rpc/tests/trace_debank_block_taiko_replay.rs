//! Pinned Mainnet and Hoodi replay proofs for Taiko block execution and state diffs.
#![allow(missing_docs, clippy::missing_docs_in_private_items)]

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::{TAIKO_HOODI, TAIKO_MAINNET, spec::TaikoChainSpec};
use alethia_reth_rpc::{
    debank::{
        BlockStorageDiff, DebankOutput, build_storage_diff, decode_and_validate_storage_diff_exact,
        decode_storage_diff_exact,
    },
    trace::{DebankTraceApiServer, DebankTraceExt},
};
use alloy_consensus::{
    Block, BlockBody, BlockHeader, Header,
    constants::{EMPTY_ROOT_HASH, KECCAK_EMPTY},
    proofs::calculate_transaction_root,
};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{
    Address, B256, Bytes, keccak256,
    map::{AddressHashMap, B256Map},
};
use alloy_rpc_types_eth::{
    Block as RpcBlock, EIP1186AccountProofResponse, TransactionReceipt as RpcReceipt,
};
use futures_util::stream;
use reth::{
    network::noop::NoopNetwork,
    tasks::{Runtime, pool::BlockingTaskPool},
};
use reth_chain_state::CanonStateNotification;
use reth_ethereum_primitives::{
    EthPrimitives, Receipt, TransactionSigned, calculate_receipt_root_no_memo,
};
use reth_evm::{
    ConfigureEvm,
    block::{BlockExecutor, TxResult},
};
use reth_execution_types::{Chain, ExecutionOutcome};
use reth_optimism_trie::InMemoryProofsStorage;
use reth_primitives_traits::{Account, RecoveredBlock, SignedTransaction, logs_bloom};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_revm::{
    State,
    database::StateProviderDatabase,
    db::states::bundle_state::{BundleRetention, BundleState},
};
use reth_rpc::EthApiBuilder;
use reth_rpc_eth_types::{EthStateCache, cache::cache_new_blocks_task};
use reth_transaction_pool::test_utils::testing_pool;
use reth_trie::{HashedPostState, KeccakKeyHasher};
use reth_trie_common::{AccountProof, DecodedMultiProofV2, Nibbles, StorageProof};
use reth_trie_sparse::{SparseStateTrie, provider::DefaultTrieNodeProviderFactory};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::Arc,
};

const FIXTURE_PAYLOAD_FILES: [&str; 7] = [
    "block.json",
    "parent.json",
    "block-hashes.json",
    "raw-transactions.json",
    "receipts.json",
    "state-proofs.json",
    "codes.json",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFixture {
    schema_version: u64,
    chain_id: u64,
    height: u64,
    block_hash: B256,
    parent_hash: B256,
    parent_state_root: B256,
    state_root: B256,
    files: BTreeMap<String, ManifestFileFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFileFixture {
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct BlockHashFixture {
    number: u64,
    hash: B256,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTransactionsFixture {
    block_hash: B256,
    transactions: Vec<RawTransactionFixture>,
}

#[derive(Debug, Deserialize)]
struct RawTransactionFixture {
    hash: B256,
    raw: Bytes,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateProofsFixture {
    block_hash: B256,
    state_root: B256,
    accounts: Vec<StateProofAccountFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateProofAccountFixture {
    role: String,
    requested_storage_keys: Vec<B256>,
    proof: EIP1186AccountProofResponse,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodesFixture {
    block_hash: B256,
    accounts: Vec<CodeFixture>,
}

#[derive(Debug, Deserialize)]
struct CodeFixture {
    role: String,
    address: Address,
    code: Bytes,
}

#[derive(Clone, Copy)]
enum FixtureNetwork {
    Mainnet,
    Hoodi,
}

impl FixtureNetwork {
    const fn directory(self) -> &'static str {
        match self {
            Self::Mainnet => "taiko-mainnet",
            Self::Hoodi => "taiko-hoodi",
        }
    }

    const fn chain_id(self) -> u64 {
        match self {
            Self::Mainnet => 167_000,
            Self::Hoodi => 167_013,
        }
    }

    fn chain_spec(self) -> Arc<TaikoChainSpec> {
        match self {
            Self::Mainnet => TAIKO_MAINNET.clone(),
            Self::Hoodi => TAIKO_HOODI.clone(),
        }
    }
}

fn fixture_dir(network: FixtureNetwork, height: u64) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(network.directory())
        .join(height.to_string())
}

fn read_json<T: for<'de> Deserialize<'de>>(network: FixtureNetwork, height: u64, name: &str) -> T {
    let path = fixture_dir(network, height).join(name);
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("failed to decode {}: {error}", path.display()))
}

fn validate_fixture_manifest(network: FixtureNetwork, height: u64) -> ManifestFixture {
    let manifest: ManifestFixture = read_json(network, height, "manifest.json");
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.chain_id, network.chain_id());
    assert_eq!(manifest.height, height);

    let expected_files = FIXTURE_PAYLOAD_FILES.into_iter().collect::<BTreeSet<_>>();
    let actual_files = manifest.files.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(actual_files, expected_files, "manifest payload file set differs");
    for (name, expected) in &manifest.files {
        let path = fixture_dir(network, height).join(name);
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        assert_eq!(bytes.len() as u64, expected.size_bytes, "manifest size differs for {name}");
        let digest = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(digest, expected.sha256, "manifest sha256 differs for {name}");
    }
    manifest
}

fn decode_transactions(
    network: FixtureNetwork,
    height: u64,
    rpc_block: &RpcBlock,
) -> (Vec<TransactionSigned>, Vec<Address>) {
    let raw: RawTransactionsFixture = read_json(network, height, "raw-transactions.json");
    assert_eq!(raw.block_hash, rpc_block.header.hash);
    let rpc_hashes = rpc_block.transactions.hashes().collect::<Vec<_>>();
    assert_eq!(
        raw.transactions.iter().map(|tx| tx.hash).collect::<Vec<_>>(),
        rpc_hashes,
        "raw transaction order must match the full RPC block"
    );

    let mut transactions = Vec::with_capacity(raw.transactions.len());
    let mut senders = Vec::with_capacity(raw.transactions.len());
    for expected in raw.transactions {
        let transaction = TransactionSigned::decode_2718_exact(expected.raw.as_ref())
            .unwrap_or_else(|error| panic!("decode raw transaction {}: {error}", expected.hash));
        assert_eq!(*transaction.tx_hash(), expected.hash);
        assert_eq!(transaction.recalculate_hash(), expected.hash);
        senders.push(
            transaction
                .try_recover()
                .unwrap_or_else(|error| panic!("recover transaction {}: {error}", expected.hash)),
        );
        transactions.push(transaction);
    }
    (transactions, senders)
}

fn decode_receipts(network: FixtureNetwork, height: u64, block_hash: B256) -> Vec<Receipt> {
    let rpc_receipts: Vec<RpcReceipt> = read_json(network, height, "receipts.json");
    rpc_receipts
        .into_iter()
        .enumerate()
        .map(|(index, receipt)| {
            assert_eq!(receipt.transaction_index, Some(index as u64));
            assert_eq!(receipt.block_hash, Some(block_hash));
            assert_eq!(receipt.block_number, Some(height));
            receipt.into_inner().into()
        })
        .collect()
}

fn load_block(
    network: FixtureNetwork,
    height: u64,
) -> (RecoveredBlock<Block<TransactionSigned>>, RpcBlock) {
    let rpc_block: RpcBlock = read_json(network, height, "block.json");
    assert_eq!(rpc_block.header.number, height);
    assert_eq!(rpc_block.header.inner.hash_slow(), rpc_block.header.hash);
    assert!(rpc_block.uncles.is_empty());

    let (transactions, senders) = decode_transactions(network, height, &rpc_block);
    assert_eq!(calculate_transaction_root(&transactions), rpc_block.header.transactions_root);
    let block = Block {
        header: rpc_block.header.inner.clone(),
        body: BlockBody {
            transactions,
            ommers: Default::default(),
            withdrawals: rpc_block.withdrawals.clone(),
        },
    };
    (RecoveredBlock::new(block, senders, rpc_block.header.hash), rpc_block)
}

fn load_parent(network: FixtureNetwork, height: u64) -> RpcBlock {
    let parent: RpcBlock = read_json(network, height, "parent.json");
    assert_eq!(parent.header.inner.hash_slow(), parent.header.hash);
    assert_eq!(parent.header.number + 1, height);
    parent
}

fn proof_account(proof: &EIP1186AccountProofResponse) -> Option<Account> {
    if proof.nonce == 0 &&
        proof.balance.is_zero() &&
        proof.code_hash.is_zero() &&
        proof.storage_hash.is_zero()
    {
        return None;
    }
    Some(Account {
        nonce: proof.nonce,
        balance: proof.balance,
        bytecode_hash: (proof.code_hash != KECCAK_EMPTY).then_some(proof.code_hash),
    })
}

fn build_execution_provider(
    network: FixtureNetwork,
    height: u64,
    parent: &RpcBlock,
) -> (
    MockEthProvider<EthPrimitives, alethia_reth_chainspec::spec::TaikoChainSpec>,
    StateProofsFixture,
) {
    let proofs: StateProofsFixture = read_json(network, height, "state-proofs.json");
    let codes: CodesFixture = read_json(network, height, "codes.json");
    assert_eq!(proofs.block_hash, parent.header.hash);
    assert_eq!(proofs.state_root, parent.header.state_root);
    assert_eq!(codes.block_hash, parent.header.hash);

    let mut code_by_address = AddressHashMap::default();
    for code in codes.accounts {
        assert!(!code.role.is_empty());
        assert!(
            code_by_address.insert(code.address, code.code).is_none(),
            "duplicate code fixture for {}",
            code.address
        );
    }
    let proof_addresses =
        proofs.accounts.iter().map(|account| account.proof.address).collect::<BTreeSet<_>>();
    assert_eq!(
        code_by_address.keys().copied().collect::<BTreeSet<_>>(),
        proof_addresses,
        "code fixture must cover exactly the proof accounts"
    );

    let provider = MockEthProvider::<EthPrimitives>::new()
        .with_chain_spec(network.chain_spec().as_ref().clone());
    let block_hashes: Vec<BlockHashFixture> = read_json(network, height, "block-hashes.json");
    assert_eq!(block_hashes.len(), 256, "BLOCKHASH fixture must cover 256 blocks");
    let mut unique_hashes = BTreeSet::new();
    for (offset, block_hash) in block_hashes.iter().enumerate() {
        assert_eq!(
            block_hash.number,
            height - 256 + offset as u64,
            "BLOCKHASH fixture must be ordered and contiguous"
        );
        assert!(
            unique_hashes.insert(block_hash.hash),
            "duplicate historical block hash {}",
            block_hash.hash
        );
        provider.add_header(
            block_hash.hash,
            Header { number: block_hash.number, ..Default::default() },
        );
    }
    let last = block_hashes.last().expect("256 block hashes are present");
    assert_eq!(last.number, parent.header.number);
    assert_eq!(last.hash, parent.header.hash);
    provider.add_header(parent.header.hash, parent.header.inner.clone());
    for account in &proofs.accounts {
        assert!(!account.role.is_empty());
        let proof = &account.proof;
        let code = code_by_address
            .remove(&proof.address)
            .unwrap_or_else(|| panic!("missing code fixture for {}", proof.address));
        let parent_account = proof_account(proof);
        if parent_account.is_none() {
            assert!(code.is_empty(), "absent parent account {} returned code", proof.address);
            assert!(
                proof.storage_proof.is_empty(),
                "absent parent account {} returned storage proofs",
                proof.address
            );
        } else {
            assert_eq!(
                keccak256(&code),
                proof.code_hash,
                "parent code hash mismatch for {} ({})",
                proof.address,
                account.role
            );
        }
        let requested = account.requested_storage_keys.iter().copied().collect::<BTreeSet<_>>();
        let proved =
            proof.storage_proof.iter().map(|slot| slot.key.as_b256()).collect::<BTreeSet<_>>();
        assert_eq!(
            requested, proved,
            "storage proof keys differ for {} ({})",
            proof.address, account.role
        );

        if parent_account.is_some() {
            let mut extended = ExtendedAccount::new(proof.nonce, proof.balance);
            if !code.is_empty() {
                extended = extended.with_bytecode(code);
            }
            extended = extended.extend_storage(
                proof.storage_proof.iter().map(|slot| (slot.key.as_b256(), slot.value)),
            );
            provider.add_account(proof.address, extended);
        }
    }
    assert!(code_by_address.is_empty());
    (provider, proofs)
}

fn verified_decoded_proof(proofs: &StateProofsFixture, parent_root: B256) -> DecodedMultiProofV2 {
    let mut witness = B256Map::default();
    for account in &proofs.accounts {
        let proof = &account.proof;
        let storage_proofs = proof
            .storage_proof
            .iter()
            .map(|slot| {
                let mut storage = StorageProof::new(slot.key.as_b256());
                storage.value = slot.value;
                storage.proof.clone_from(&slot.proof);
                storage
            })
            .collect();
        let info = proof_account(proof);
        let storage_root = if info.is_none() { EMPTY_ROOT_HASH } else { proof.storage_hash };
        let account_proof = AccountProof {
            address: proof.address,
            info,
            proof: proof.account_proof.clone(),
            storage_root,
            storage_proofs,
        };
        account_proof.verify(parent_root).unwrap_or_else(|error| {
            panic!("invalid account/storage proof for {}: {error}", proof.address)
        });

        for node in proof
            .account_proof
            .iter()
            .chain(proof.storage_proof.iter().flat_map(|slot| &slot.proof))
        {
            let node_hash = keccak256(node);
            if let Some(existing) = witness.insert(node_hash, node.clone()) {
                assert_eq!(existing, *node, "same trie node hash has different bytes");
            }
        }
    }
    DecodedMultiProofV2::from_witness(parent_root, &witness)
        .expect("decode the exact EIP-1186 witness")
}

fn assert_bundle_witness_coverage(bundle: &BundleState, proofs: &StateProofsFixture) {
    let requested_by_address = proofs
        .accounts
        .iter()
        .map(|account| {
            (
                account.proof.address,
                account.requested_storage_keys.iter().copied().collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut missing = Vec::new();
    for (address, account) in bundle.state() {
        if let Some(requested) = requested_by_address.get(address) {
            for slot in account.storage.keys() {
                let slot = B256::from(*slot);
                if !requested.contains(&slot) {
                    missing.push(format!("{address}/{slot}"));
                }
            }
        } else {
            missing.push(format!("{address}/*"));
        }
    }
    assert!(missing.is_empty(), "bundle state is absent from the witness: {missing:?}");
}

fn sparse_root(decoded_proof: DecodedMultiProofV2, post_state: &HashedPostState) -> B256 {
    let provider = DefaultTrieNodeProviderFactory;
    let mut trie = SparseStateTrie::new();
    trie.reveal_decoded_multiproof_v2(decoded_proof).expect("reveal the verified parent proof");

    for (address, storage) in &post_state.storages {
        assert!(
            trie.is_account_revealed(*address),
            "state diff storage account {address} is absent from the witness"
        );
        for slot in storage.storage.keys() {
            assert!(
                trie.check_valid_storage_witness(*address, *slot),
                "state diff storage slot {address}/{slot} is absent from the witness"
            );
        }
    }
    for address in post_state.accounts.keys() {
        assert!(
            trie.is_account_revealed(*address),
            "state diff account {address} is absent from the witness"
        );
    }

    for (address, storage) in &post_state.storages {
        if storage.wiped {
            trie.wipe_storage(*address).expect("wipe revealed storage");
        }
        for (slot, value) in &storage.storage {
            let path = Nibbles::unpack(*slot);
            if value.is_zero() {
                trie.remove_storage_leaf(*address, &path, &provider)
                    .expect("remove revealed storage leaf");
            } else {
                trie.update_storage_leaf(
                    *address,
                    path,
                    alloy_rlp::encode_fixed_size(value).to_vec(),
                    &provider,
                )
                .expect("update revealed storage leaf");
            }
        }
    }
    for (address, account) in &post_state.accounts {
        match account {
            None => trie
                .remove_account_leaf(&Nibbles::unpack(*address), &provider)
                .expect("remove revealed account"),
            Some(account) => {
                let keep = trie
                    .update_account(*address, *account, &provider)
                    .expect("update revealed account");
                if !keep {
                    trie.remove_account_leaf(&Nibbles::unpack(*address), &provider)
                        .expect("remove empty account");
                }
            }
        }
    }
    trie.root(&provider).expect("calculate sparse state root without database fallback")
}

async fn cache_replay_block(
    provider: &MockEthProvider<EthPrimitives, TaikoChainSpec>,
    block: RecoveredBlock<Block<TransactionSigned>>,
    receipts: Vec<Receipt>,
) -> EthStateCache<EthPrimitives> {
    let cache = EthStateCache::spawn_with(provider.clone(), Default::default(), Runtime::test());
    let block_number = block.number();
    let outcome = ExecutionOutcome::new(
        Default::default(),
        vec![receipts],
        block_number,
        vec![Default::default()],
    );
    let notification = CanonStateNotification::Commit {
        new: Arc::new(Chain::new(vec![block], outcome, Default::default())),
    };
    cache_new_blocks_task(cache.clone(), stream::iter([notification])).await;
    cache
}

async fn trace_fixture_through_handler(
    network: FixtureNetwork,
    height: u64,
    expected_state_diff: &BlockStorageDiff,
) {
    let (block, rpc_block) = load_block(network, height);
    let parent = load_parent(network, height);
    let stored_receipts = decode_receipts(network, height, block.hash());
    let (provider, proofs) = build_execution_provider(network, height, &parent);

    provider.add_block(block.hash(), block.clone().into_block());
    provider.add_receipts(height, stored_receipts.clone());
    // `MockEthProvider` is only the handler transport harness. The formal replay above and the
    // EIP-1186 sparse-trie check below independently prove the roots and expected state diff.
    // Its LIFO root queue proves that the handler performs parent, replay-A, and decoded-B checks.
    provider.state_roots.lock().extend([
        rpc_block.header.state_root,
        rpc_block.header.state_root,
        parent.header.state_root,
    ]);

    let cache = cache_replay_block(&provider, block.clone(), stored_receipts.clone()).await;
    let eth = EthApiBuilder::new(
        provider.clone(),
        testing_pool(),
        NoopNetwork::default(),
        TaikoEvmConfig::new(network.chain_spec()),
    )
    .eth_cache(cache)
    .blocking_task_pool(BlockingTaskPool::new(
        BlockingTaskPool::builder().num_threads(1).build().unwrap(),
    ))
    .build();
    let module = DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1)
        .with_state_root_verification(true)
        .into_rpc();
    let block_id = serde_json::to_string(&alloy_eips::BlockId::hash_canonical(block.hash()))
        .expect("serialize exact canonical block id");
    let request =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"trace_debankBlock","params":[{block_id}]}}"#);
    let (raw_response, _) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        module.raw_json_request(&request, 1),
    )
    .await
    .expect("trace_debankBlock handler timed out")
    .expect("trace_debankBlock raw request failed");
    let response: serde_json::Value =
        serde_json::from_str(raw_response.get()).expect("handler response must be JSON");

    let response = response.as_object().expect("JSON-RPC response must be an object");
    assert_eq!(response.len(), 3, "response must contain jsonrpc, id, and result only");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(!response.contains_key("error"), "{response:#?}");
    let result = response["result"].as_object().expect("result must be a complete JSON object");
    assert_eq!(
        result.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["block_file", "header", "state_diff", "validation_hash"])
    );
    let block_file = result["block_file"].as_object().expect("block_file must be an object");
    assert_eq!(
        block_file.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "block",
            "error_events",
            "error_traces",
            "events",
            "storage_contracts",
            "traces",
            "txs",
        ])
    );

    let output: DebankOutput =
        serde_json::from_value(serde_json::Value::Object(result.clone())).unwrap();
    assert_eq!(output.block_file.block.id, block.hash());
    assert_eq!(output.block_file.block.height, height);
    assert_eq!(output.block_file.block.parent_id, parent.header.hash);
    assert_eq!(output.block_file.block.gas_limit, block.gas_limit());
    assert_eq!(output.block_file.block.gas_used, block.gas_used());
    assert_eq!(output.block_file.block.timestamp, block.timestamp());
    assert_eq!(output.block_file.transactions.len(), block.body().transactions.len());
    assert_eq!(
        output.block_file.events.len(),
        stored_receipts.iter().map(|receipt| receipt.logs.len()).sum::<usize>()
    );

    let mut prior_cumulative_gas = 0;
    for ((wire_transaction, transaction), receipt) in output
        .block_file
        .transactions
        .iter()
        .zip(block.transactions_recovered())
        .zip(&stored_receipts)
    {
        assert_eq!(wire_transaction.id, transaction.tx_hash().to_string());
        assert_eq!(wire_transaction.status, receipt.success);
        assert_eq!(wire_transaction.gas_used, receipt.cumulative_gas_used - prior_cumulative_gas);
        prior_cumulative_gas = receipt.cumulative_gas_used;
    }
    assert_eq!(prior_cumulative_gas, block.gas_used());

    let root_trace_ids = output
        .block_file
        .traces
        .iter()
        .chain(&output.block_file.error_traces)
        .filter(|trace| trace.parent_trace_id.is_empty())
        .map(|trace| trace.tx_id.as_str())
        .collect::<BTreeSet<_>>();
    let transaction_ids = block
        .body()
        .transactions
        .iter()
        .map(|transaction| transaction.tx_hash().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        root_trace_ids,
        transaction_ids.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        "formatter must emit exactly one root trace per transaction"
    );
    assert_eq!(output.validation_hash, output.block_file.validation().validation_hash);

    assert_eq!(output.header.number, height);
    assert_eq!(output.header.hash, rpc_block.header.hash);
    assert_eq!(output.header.parent_hash, rpc_block.header.parent_hash);
    assert_eq!(output.header.state_root, rpc_block.header.state_root);
    assert_eq!(output.header.transactions_root, rpc_block.header.transactions_root);
    assert_eq!(output.header.receipts_root, rpc_block.header.receipts_root);
    assert_eq!(output.header.logs_bloom, rpc_block.header.logs_bloom);
    assert_eq!(output.header.gas_used, rpc_block.header.gas_used);
    assert_eq!(output.header.difficulty, rpc_block.header.difficulty);

    let header_json = result["header"].as_object().expect("header must be an object");
    assert!(!header_json.contains_key("requestsHash"));
    if let Some(requests_hash) = rpc_block.header.requests_hash {
        assert_eq!(header_json["requestsRoot"], requests_hash.to_string());
    } else {
        assert!(!header_json.contains_key("requestsRoot"));
    }

    let (decoded_diff, decoded_state) =
        decode_storage_diff_exact(&output.state_diff).expect("decode exact handler state diff");
    assert_eq!(&decoded_diff, expected_state_diff);
    assert_eq!(decoded_diff.parent_hash, parent.header.state_root);
    assert_eq!(decoded_diff.hash, rpc_block.header.state_root);
    let decoded_proof = verified_decoded_proof(&proofs, parent.header.state_root);
    assert_eq!(
        sparse_root(decoded_proof, &decoded_state),
        rpc_block.header.state_root,
        "handler state_diff must independently rebuild the canonical root"
    );
    assert!(
        provider.state_roots.lock().is_empty(),
        "handler must consume exactly parent, replay-A, and decoded-B root checks"
    );
}

fn replay_fixture(network: FixtureNetwork, height: u64) -> BlockStorageDiff {
    let manifest = validate_fixture_manifest(network, height);
    let (block, rpc_block) = load_block(network, height);
    let parent = load_parent(network, height);
    assert_eq!(manifest.block_hash, block.hash());
    assert_eq!(manifest.parent_hash, parent.header.hash);
    assert_eq!(manifest.parent_state_root, parent.header.state_root);
    assert_eq!(manifest.state_root, block.state_root());
    assert_eq!(block.parent_hash(), parent.header.hash);

    let stored_receipts = decode_receipts(network, height, block.hash());
    assert_eq!(stored_receipts.len(), block.body().transactions.len());
    assert_eq!(calculate_receipt_root_no_memo(&stored_receipts), block.receipts_root());
    assert_eq!(
        logs_bloom(stored_receipts.iter().flat_map(|receipt| &receipt.logs)),
        block.logs_bloom()
    );
    assert_eq!(
        stored_receipts.last().map_or(0, |receipt| receipt.cumulative_gas_used),
        block.gas_used()
    );

    let (provider, proofs) = build_execution_provider(network, height, &parent);
    let db = StateProviderDatabase::new(&provider);
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    let evm_config = TaikoEvmConfig::new(network.chain_spec());
    let mut executor = evm_config
        .executor_for_block(&mut state, block.sealed_block())
        .expect("construct the formal Taiko block executor");
    executor.apply_pre_execution_changes().expect("apply Taiko pre-execution");
    for (index, transaction) in block.transactions_recovered().enumerate() {
        let output = executor
            .execute_transaction_without_commit(transaction)
            .expect("replay canonical transaction");
        assert_eq!(
            output.result().result.is_success(),
            stored_receipts[index].success,
            "transaction {} result differs from its stored receipt: {:?}",
            transaction.tx_hash(),
            output.result().result
        );
        executor.commit_transaction(output).expect("commit canonical transaction");
    }
    let (_, execution) = executor.finish().expect("finish canonical Taiko replay");
    assert_eq!(execution.receipts, stored_receipts);
    assert_eq!(execution.gas_used, block.gas_used());

    state.merge_transitions(BundleRetention::PlainState);
    let bundle = state.take_bundle();
    assert_bundle_witness_coverage(&bundle, &proofs);
    let replay_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
    let decoded_proof = verified_decoded_proof(&proofs, parent.header.state_root);
    let root_a = sparse_root(decoded_proof.clone(), &replay_state);
    assert_eq!(root_a, rpc_block.header.state_root);

    let state_diff =
        build_storage_diff(&bundle, root_a, parent.header.state_root).expect("build state diff");
    let wire = alloy_rlp::encode(&state_diff);
    let decoded_state = decode_and_validate_storage_diff_exact(&wire, &state_diff)
        .expect("decode exact state diff");
    let root_b = sparse_root(decoded_proof, &decoded_state);
    assert_eq!(root_b, root_a);
    assert_eq!(root_b, rpc_block.header.state_root);
    state_diff
}

#[test]
fn anchor_plus_regular_mainnet_block_replays_to_header_root() {
    replay_fixture(FixtureNetwork::Mainnet, 9_069_694);
}

#[test]
fn anchor_only_mainnet_block_replays_to_header_root() {
    replay_fixture(FixtureNetwork::Mainnet, 9_069_705);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hoodi_unzen_boundary_replays_through_handler() {
    for height in [11_981_107, 11_981_108, 11_981_109] {
        let expected = replay_fixture(FixtureNetwork::Hoodi, height);
        trace_fixture_through_handler(FixtureNetwork::Hoodi, height, &expected).await;
    }
}
