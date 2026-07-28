//! `trace_debankBlock` implementation for Taiko's production executor.

use crate::{
    debank::{
        BlockFile, DebankBlock, DebankOutput, DebankTransaction, build_debank_traces,
        build_genesis_storage_diff, build_rpc_header, build_storage_diff, complete_state_root,
        decode_and_validate_storage_diff_exact, genesis_storage_contracts,
        observed_storage_contracts,
    },
    geth_error::GethErrorInspector,
    proof_state::ProofHistoryStateProviderFactory,
};
use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::hardfork::TaikoHardforks;
#[cfg(test)]
use alloy_consensus::constants::EMPTY_WITHDRAWALS;
use alloy_consensus::{
    BlockHeader, Transaction, TxReceipt,
    constants::{EMPTY_OMMER_ROOT_HASH, EMPTY_RECEIPTS, EMPTY_TRANSACTIONS},
    transaction::TxHashRef,
};
use alloy_eips::{BlockId, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::{Address, Bloom, TxKind};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObjectOwned};
use reth::{chainspec::EthChainSpec, revm::cancelled::CancelOnDrop};
use reth_ethereum::EthPrimitives;
use reth_evm::{ConfigureEvm, Evm, block::BlockExecutor};
use reth_optimism_trie::OpProofsStore;
use reth_primitives_traits::{BlockBody, RecoveredBlock, logs_bloom};
use reth_provider::{
    BlockHashReader, ChainSpecProvider, HeaderProvider, StateProvider, StateProviderFactory,
    StateRootProvider,
};
use reth_revm::{
    State, database::StateProviderDatabase, db::states::bundle_state::BundleRetention,
};
use reth_rpc_eth_api::{RpcNodeCore, helpers::FullEthApi};
use reth_tasks::pool::BlockingTaskGuard;
use reth_trie::{HashedPostState, KeccakKeyHasher};
use revm_inspectors::tracing::{OpcodeFilter, TracingInspector, TracingInspectorConfig};
use serde_json::value::RawValue;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::OwnedSemaphorePermit,
    time::{Instant, timeout_at},
};

/// End-to-end deadline for a replay request.
const DEBANK_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// RPC error code for blocks that changed canonical identity during replay.
const BLOCK_REORGED: i32 = -32_010;
/// RPC error code for unavailable exact-hash history.
const BLOCK_OR_HISTORY_UNAVAILABLE: i32 = -32_011;
/// RPC error code for replay artifacts that disagree with stored consensus data.
const EXECUTION_CONSENSUS_MISMATCH: i32 = -32_012;
/// RPC error code for a deadline or disconnected-client cancellation.
const REQUEST_CANCELLED: i32 = -32_013;

/// RPC interface for the DeBank block tracer.
#[rpc(server, namespace = "trace")]
pub trait DebankTraceApi {
    /// Replays one canonical block and returns its validated DeBank payload.
    #[method(name = "debankBlock")]
    async fn debank_block(&self, block_id: BlockId) -> RpcResult<Box<RawValue>>;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayTestPhase {
    InitialCanonicalChecked,
    ProviderLoaded,
    PreExecutionApplied,
    TransactionExecuted,
    BeforeFormatter,
    FormatterCompleted,
    RootAComputed,
    StateDiffEncoded,
    StateDiffDecoded,
    RootBComputed,
    OutputBuilt,
    JsonSerializationCompleted,
    JsonSerialized,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum ReplayTestResource {
    Worker,
    Permit,
    ProviderReader,
    OutputBuffer,
    SerializedBuffer,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReplayTestResourceSnapshot {
    workers: usize,
    permits: usize,
    provider_readers: usize,
    output_buffers: usize,
    serialized_buffers: usize,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct ReplayTestResourceCounters {
    workers: std::sync::atomic::AtomicUsize,
    permits: std::sync::atomic::AtomicUsize,
    provider_readers: std::sync::atomic::AtomicUsize,
    output_buffers: std::sync::atomic::AtomicUsize,
    serialized_buffers: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
struct ReplayTestResources(Arc<ReplayTestResourceCounters>);

#[cfg(test)]
impl ReplayTestResources {
    fn guard(&self, resource: ReplayTestResource) -> ReplayTestResourceGuard {
        self.counter(resource).fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ReplayTestResourceGuard { resources: self.clone(), resource }
    }

    fn snapshot(&self) -> ReplayTestResourceSnapshot {
        let ordering = std::sync::atomic::Ordering::SeqCst;
        ReplayTestResourceSnapshot {
            workers: self.0.workers.load(ordering),
            permits: self.0.permits.load(ordering),
            provider_readers: self.0.provider_readers.load(ordering),
            output_buffers: self.0.output_buffers.load(ordering),
            serialized_buffers: self.0.serialized_buffers.load(ordering),
        }
    }

    fn counter(&self, resource: ReplayTestResource) -> &std::sync::atomic::AtomicUsize {
        match resource {
            ReplayTestResource::Worker => &self.0.workers,
            ReplayTestResource::Permit => &self.0.permits,
            ReplayTestResource::ProviderReader => &self.0.provider_readers,
            ReplayTestResource::OutputBuffer => &self.0.output_buffers,
            ReplayTestResource::SerializedBuffer => &self.0.serialized_buffers,
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
struct ReplayTestResourceGuard {
    resources: ReplayTestResources,
    resource: ReplayTestResource,
}

#[cfg(test)]
impl Drop for ReplayTestResourceGuard {
    fn drop(&mut self) {
        let previous =
            self.resources.counter(self.resource).fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        debug_assert!(previous > 0, "replay test resource counter underflow");
    }
}

#[cfg(test)]
#[derive(Clone)]
struct ReplayTestProbe {
    on_worker_enter: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    on_phase: Option<Arc<dyn Fn(ReplayTestPhase) + Send + Sync>>,
    resources: Option<ReplayTestResources>,
}

#[cfg(test)]
impl std::fmt::Debug for ReplayTestProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayTestProbe").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl ReplayTestProbe {
    fn new(on_worker_enter: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self { on_worker_enter: Some(Arc::new(on_worker_enter)), on_phase: None, resources: None }
    }

    fn for_phase(on_phase: impl Fn(ReplayTestPhase) + Send + Sync + 'static) -> Self {
        Self { on_worker_enter: None, on_phase: Some(Arc::new(on_phase)), resources: None }
    }

    fn with_resources(mut self, resources: ReplayTestResources) -> Self {
        self.resources = Some(resources);
        self
    }

    fn worker_entered(&self, block_number: u64) {
        if let Some(on_worker_enter) = self.on_worker_enter.as_ref() {
            on_worker_enter(block_number);
        }
    }

    fn phase_reached(&self, phase: ReplayTestPhase) {
        if let Some(on_phase) = self.on_phase.as_ref() {
            on_phase(phase);
        }
    }

    fn resource_guard(&self, resource: ReplayTestResource) -> Option<ReplayTestResourceGuard> {
        self.resources.as_ref().map(|resources| resources.guard(resource))
    }
}

/// Replay permit retained until the handler completes its final canonical check.
#[cfg(not(test))]
type ReplayPermit = OwnedSemaphorePermit;

/// Test permit wrapper whose guard follows the underlying semaphore permit.
#[cfg(test)]
#[derive(Debug)]
struct ReplayPermit {
    /// Underlying replay semaphore permit.
    _permit: OwnedSemaphorePermit,
    /// Test-only live-resource guard.
    _resource: Option<ReplayTestResourceGuard>,
}

/// Serialized replay result and the permit retained by the RPC handler.
struct ReplayResponse {
    /// Pre-serialized method result.
    serialized: Box<RawValue>,
    /// Permit retained until final canonical validation completes.
    _permit: ReplayPermit,
    /// Test-only handler-owned serialized buffer guard.
    #[cfg(test)]
    _serialized_resource: Option<ReplayTestResourceGuard>,
}

/// Taiko DeBank tracer backed by the configured Ethereum RPC API.
#[derive(Debug)]
pub struct DebankTraceExt<Eth, Storage> {
    /// Ethereum RPC helper used for exact block, receipt, state, and EVM access.
    eth: Eth,
    /// Optional proof-history overlay used for exact deep historical state.
    proof_history: Option<ProofHistoryStateProviderFactory<Eth, Storage>>,
    /// Limits concurrent DeBank replays independently from response delivery.
    replay_guard: BlockingTaskGuard,
    /// Whether replay requests compute and verify parent and post-block state roots.
    verify_state_roots: bool,
    /// Optional replay worker barrier used by concurrency tests.
    #[cfg(test)]
    replay_test_probe: Option<ReplayTestProbe>,
    /// Request timeout override used by deadline tests.
    #[cfg(test)]
    request_timeout: Duration,
}

impl<Eth, Storage> DebankTraceExt<Eth, Storage> {
    /// Creates a DeBank tracer with the configured replay concurrency limit.
    pub fn new(
        eth: Eth,
        proof_history: Option<ProofHistoryStateProviderFactory<Eth, Storage>>,
        max_concurrent_replays: usize,
    ) -> Self {
        Self {
            eth,
            proof_history,
            replay_guard: BlockingTaskGuard::new(max_concurrent_replays),
            verify_state_roots: false,
            #[cfg(test)]
            replay_test_probe: None,
            #[cfg(test)]
            request_timeout: DEBANK_REQUEST_TIMEOUT,
        }
    }

    /// Configures whether replay requests compute and verify state roots.
    pub fn with_state_root_verification(mut self, verify_state_roots: bool) -> Self {
        self.verify_state_roots = verify_state_roots;
        self
    }

    #[cfg(test)]
    fn with_replay_test_probe(mut self, probe: ReplayTestProbe) -> Self {
        self.replay_test_probe = Some(probe);
        self
    }

    #[cfg(test)]
    fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Waits for a replay slot without coupling its lifetime to HTTP response delivery.
    async fn acquire_replay_permit(&self, deadline: Instant) -> RpcResult<ReplayPermit> {
        let permit = timeout_at(deadline, self.replay_guard.clone().acquire_owned())
            .await
            .map_err(|_| {
                rpc_error(REQUEST_CANCELLED, "REQUEST_CANCELLED", "tracing permit deadline")
            })?
            .map_err(|error| {
                rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
            })?;
        #[cfg(test)]
        let permit = ReplayPermit {
            _permit: permit,
            _resource: self
                .replay_test_probe
                .as_ref()
                .and_then(|probe| probe.resource_guard(ReplayTestResource::Permit)),
        };
        Ok(permit)
    }
}

/// Builds a stable JSON-RPC error object with a machine-readable reason.
fn rpc_error(code: i32, reason: &'static str, detail: impl ToString) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(code, reason, Some(detail.to_string()))
}

/// Requires the complete EVM `BLOCKHASH` window from the exact parent-state provider.
fn validate_block_hash_window(
    provider: &dyn StateProvider,
    block_number: u64,
    expected_parent_hash: alloy_primitives::B256,
) -> RpcResult<()> {
    let start = block_number.saturating_sub(256);
    let hashes = provider.canonical_hashes_range(start, block_number).map_err(|error| {
        rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
    })?;
    let expected_len =
        usize::try_from(block_number - start).expect("BLOCKHASH window is at most 256");
    if hashes.len() != expected_len {
        return Err(rpc_error(
            BLOCK_OR_HISTORY_UNAVAILABLE,
            "BLOCK_OR_HISTORY_UNAVAILABLE",
            format!(
                "block {block_number} requires {expected_len} canonical hashes in \
                 [{start}, {block_number}), got {}",
                hashes.len()
            ),
        ));
    }
    if hashes.last() != Some(&expected_parent_hash) {
        return Err(rpc_error(
            BLOCK_OR_HISTORY_UNAVAILABLE,
            "BLOCK_OR_HISTORY_UNAVAILABLE",
            format!(
                "block {block_number} expects parent {expected_parent_hash}, history ends at {:?}",
                hashes.last()
            ),
        ));
    }
    Ok(())
}

#[async_trait]
impl<Eth, Storage> DebankTraceApiServer for DebankTraceExt<Eth, Storage>
where
    Eth: FullEthApi + RpcNodeCore<Primitives = EthPrimitives, Evm = TaikoEvmConfig> + 'static,
    Eth::Provider: BlockHashReader + ChainSpecProvider + StateProviderFactory,
    <Eth::Provider as ChainSpecProvider>::ChainSpec: TaikoHardforks,
    Storage: OpProofsStore + Clone + 'static,
{
    async fn debank_block(&self, block_id: BlockId) -> RpcResult<Box<RawValue>> {
        if block_id.is_pending() {
            return Err(rpc_error(
                BLOCK_OR_HISTORY_UNAVAILABLE,
                "BLOCK_OR_HISTORY_UNAVAILABLE",
                "pending is not a stable replay target",
            ));
        }
        #[cfg(not(test))]
        let request_timeout = DEBANK_REQUEST_TIMEOUT;
        #[cfg(test)]
        let request_timeout = self.request_timeout;
        let deadline = Instant::now() + request_timeout;
        let permit = self.acquire_replay_permit(deadline).await?;

        let loaded = timeout_at(deadline, self.eth.load_block_and_receipts(block_id))
            .await
            .map_err(|_| rpc_error(REQUEST_CANCELLED, "REQUEST_CANCELLED", "provider deadline"))?
            .map_err(|error| {
                rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
            })?;
        let Some((block, receipts)) = loaded else {
            return Err(rpc_error(
                BLOCK_OR_HISTORY_UNAVAILABLE,
                "BLOCK_OR_HISTORY_UNAVAILABLE",
                block_id,
            ));
        };
        let expected_hash = block.hash();
        let number = block.number();
        self.assert_canonical(number, expected_hash)?;
        let result = async {
            #[cfg(test)]
            if let Some(probe) = self.replay_test_probe.as_ref() {
                probe.phase_reached(ReplayTestPhase::InitialCanonicalChecked);
            }
            let is_unzen_active =
                self.eth.provider().chain_spec().is_unzen_active(block.header().timestamp());
            validate_stored_consensus(&block, &receipts, is_unzen_active)?;

            if block.body().transactions().any(is_unsupported_debank_transaction) {
                return Err(rpc_error(
                    EXECUTION_CONSENSUS_MISMATCH,
                    "EXECUTION_CONSENSUS_MISMATCH",
                    "transaction type 3 is unsupported by Taiko",
                ));
            }

            let response = if number == 0 {
                let output = self.genesis_output(&block, &receipts)?;
                let cancel = CancelOnDrop::default();
                let serialization = self.serialize_output(output, cancel.clone(), permit);
                tokio::pin!(serialization);
                tokio::select! {
                    result = &mut serialization => result?,
                    _ = tokio::time::sleep_until(deadline) => {
                        drop(cancel);
                        return Err(rpc_error(
                            REQUEST_CANCELLED,
                            "REQUEST_CANCELLED",
                            "serialization deadline",
                        ));
                    }
                }
            } else {
                let cancel = CancelOnDrop::default();
                let replay = self.replay_output(
                    block.clone(),
                    receipts.clone(),
                    is_unzen_active,
                    cancel.clone(),
                    permit,
                );
                tokio::pin!(replay);
                tokio::select! {
                    result = &mut replay => result?,
                    _ = tokio::time::sleep_until(deadline) => {
                        drop(cancel);
                        return Err(rpc_error(
                            REQUEST_CANCELLED,
                            "REQUEST_CANCELLED",
                            "replay deadline",
                        ));
                    }
                }
            };
            if Instant::now() >= deadline {
                return Err(rpc_error(
                    REQUEST_CANCELLED,
                    "REQUEST_CANCELLED",
                    "final canonical-check deadline",
                ));
            }
            self.assert_canonical(number, expected_hash)?;
            Ok(response.serialized)
        }
        .await;
        result.map_err(|error| self.prefer_reorg_error(number, expected_hash, error))
    }
}

impl<Eth, Storage> DebankTraceExt<Eth, Storage>
where
    Eth: FullEthApi + RpcNodeCore<Primitives = EthPrimitives, Evm = TaikoEvmConfig> + 'static,
    Eth::Provider: BlockHashReader + StateProviderFactory,
    Storage: OpProofsStore + Clone + 'static,
{
    /// Verifies that the originally resolved hash is still canonical at its height.
    fn assert_canonical(&self, number: u64, expected: alloy_primitives::B256) -> RpcResult<()> {
        let canonical = self.eth.provider().block_hash(number).map_err(|error| {
            rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
        })?;
        if canonical != Some(expected) {
            return Err(rpc_error(
                BLOCK_REORGED,
                "BLOCK_REORGED",
                format!("height {number}: expected {expected}, got {canonical:?}"),
            ));
        }
        Ok(())
    }

    /// Rechecks the target identity before returning a post-resolution error.
    fn prefer_reorg_error(
        &self,
        number: u64,
        expected: alloy_primitives::B256,
        original: ErrorObjectOwned,
    ) -> ErrorObjectOwned {
        match self.assert_canonical(number, expected) {
            Err(error) if error.code() == BLOCK_REORGED => error,
            _ => original,
        }
    }

    /// Builds and validates the allocation-derived Genesis response.
    fn genesis_output(
        &self,
        block: &Arc<RecoveredBlock<reth_ethereum_primitives::Block>>,
        receipts: &[reth_ethereum_primitives::Receipt],
    ) -> RpcResult<DebankOutput> {
        if block.body().transactions().next().is_some() ||
            !receipts.is_empty() ||
            block.header().transactions_root() != EMPTY_TRANSACTIONS ||
            block.header().receipts_root() != EMPTY_RECEIPTS ||
            block.header().logs_bloom() != Bloom::ZERO ||
            block.header().gas_used() != 0
        {
            return Err(rpc_error(
                EXECUTION_CONSENSUS_MISMATCH,
                "EXECUTION_CONSENSUS_MISMATCH",
                "Genesis body, receipts, roots, logsBloom, or gasUsed is non-empty",
            ));
        }
        let chain_spec = self.eth.provider().chain_spec();
        let genesis = chain_spec.genesis();
        if block.state_root() != chain_spec.genesis_header().state_root() {
            return Err(rpc_error(
                EXECUTION_CONSENSUS_MISMATCH,
                "EXECUTION_CONSENSUS_MISMATCH",
                "stored Genesis stateRoot differs from configured chain allocation",
            ));
        }
        let state_diff = build_genesis_storage_diff(genesis, block.state_root());
        let encoded = alloy_rlp::encode(&state_diff);
        let decoded_state =
            decode_and_validate_storage_diff_exact(&encoded, &state_diff).map_err(|error| {
                rpc_error(EXECUTION_CONSENSUS_MISMATCH, "EXECUTION_CONSENSUS_MISMATCH", error)
            })?;
        let decoded_root = complete_state_root(&state_diff).map_err(|error| {
            rpc_error(EXECUTION_CONSENSUS_MISMATCH, "EXECUTION_CONSENSUS_MISMATCH", error)
        })?;
        if decoded_state.accounts.len() != genesis.alloc.len() || decoded_root != block.state_root()
        {
            return Err(rpc_error(
                EXECUTION_CONSENSUS_MISMATCH,
                "EXECUTION_CONSENSUS_MISMATCH",
                format!(
                    "Genesis decoded state is incomplete or has root {decoded_root}, expected {}",
                    block.state_root()
                ),
            ));
        }
        let block_file = BlockFile {
            block: DebankBlock::from(block.as_ref()),
            storage_contracts: genesis_storage_contracts(genesis),
            ..Default::default()
        };
        let validation_hash = block_file.validation().validation_hash;
        Ok(DebankOutput {
            block_file,
            header: build_rpc_header(block),
            state_diff: encoded.into(),
            validation_hash,
        })
    }

    /// Serializes a validated output on the tracing pool while retaining the replay permit.
    async fn serialize_output(
        &self,
        output: DebankOutput,
        cancel: CancelOnDrop,
        permit: ReplayPermit,
    ) -> RpcResult<ReplayResponse> {
        #[cfg(test)]
        let replay_test_probe = self.replay_test_probe.clone();
        #[cfg(test)]
        let output_resource = replay_test_probe
            .as_ref()
            .and_then(|probe| probe.resource_guard(ReplayTestResource::OutputBuffer));
        self.eth
            .spawn_tracing(move |_| {
                Ok((|| -> RpcResult<ReplayResponse> {
                    #[cfg(test)]
                    let _worker_resource = replay_test_probe
                        .as_ref()
                        .and_then(|probe| probe.resource_guard(ReplayTestResource::Worker));
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::OutputBuilt);
                    }
                    check_cancelled(&cancel, "before JSON serialization")?;
                    let serialized = serde_json::value::to_raw_value(&output).map_err(|error| {
                        rpc_error(
                            EXECUTION_CONSENSUS_MISMATCH,
                            "EXECUTION_CONSENSUS_MISMATCH",
                            error,
                        )
                    })?;
                    #[cfg(test)]
                    let serialized_resource = replay_test_probe.as_ref().and_then(|probe| {
                        probe.resource_guard(ReplayTestResource::SerializedBuffer)
                    });
                    drop(output);
                    #[cfg(test)]
                    drop(output_resource);
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::JsonSerializationCompleted);
                    }
                    check_cancelled(&cancel, "after JSON serialization")?;
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::JsonSerialized);
                    }
                    Ok(ReplayResponse {
                        serialized,
                        _permit: permit,
                        #[cfg(test)]
                        _serialized_resource: serialized_resource,
                    })
                })())
            })
            .await
            .map_err(|error| {
                rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
            })?
    }

    /// Replays a non-Genesis block on the tracing pool and optionally verifies its state roots.
    async fn replay_output(
        &self,
        block: Arc<RecoveredBlock<reth_ethereum_primitives::Block>>,
        stored_receipts: Arc<Vec<reth_ethereum_primitives::Receipt>>,
        is_unzen_active: bool,
        cancel: CancelOnDrop,
        permit: ReplayPermit,
    ) -> RpcResult<ReplayResponse> {
        let parent_hash = block.parent_hash();
        let parent_id = BlockId::hash_canonical(parent_hash);
        let (parent_number, canonical_state) = if let Some(factory) = self.proof_history.as_ref() {
            factory.resolve_block_state(parent_id).await.map_err(|error| {
                rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
            })?
        } else {
            let parent_number = block.number().checked_sub(1).ok_or_else(|| {
                rpc_error(
                    EXECUTION_CONSENSUS_MISMATCH,
                    "EXECUTION_CONSENSUS_MISMATCH",
                    "non-Genesis replay has no parent height",
                )
            })?;
            let state =
                self.eth.provider().history_by_block_hash(parent_hash).map_err(|error| {
                    rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
                })?;
            (parent_number, state)
        };
        #[cfg(test)]
        let provider_resource = self
            .replay_test_probe
            .as_ref()
            .and_then(|probe| probe.resource_guard(ReplayTestResource::ProviderReader));
        if parent_number.checked_add(1) != Some(block.number()) {
            return Err(rpc_error(
                EXECUTION_CONSENSUS_MISMATCH,
                "EXECUTION_CONSENSUS_MISMATCH",
                format!("parent height {parent_number} does not precede block {}", block.number()),
            ));
        }
        let proof_history = self.proof_history.clone();
        let verify_state_roots = self.verify_state_roots;
        #[cfg(test)]
        let replay_test_probe = self.replay_test_probe.clone();
        self.eth
            .spawn_tracing(move |this| {
                Ok((|| -> RpcResult<ReplayResponse> {
                    #[cfg(test)]
                    let _worker_resource = replay_test_probe
                        .as_ref()
                        .and_then(|probe| probe.resource_guard(ReplayTestResource::Worker));
                    #[cfg(test)]
                    let _provider_resource = provider_resource;
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.worker_entered(block.number());
                    }
                    check_cancelled(&cancel, "before provider load")?;
                    // Archive history serves replay reads directly; proof-history is only required
                    // when this request will compute historical state roots.
                    let state_provider: Box<dyn StateProvider + '_> =
                        if verify_state_roots &&
                            let Some(factory) = proof_history.as_ref()
                        {
                            factory.state_provider_at(canonical_state, parent_number).map_err(
                                |error| {
                                    rpc_error(
                                        BLOCK_OR_HISTORY_UNAVAILABLE,
                                        "BLOCK_OR_HISTORY_UNAVAILABLE",
                                        error,
                                    )
                                },
                            )?
                        } else {
                            canonical_state
                        };
                    let parent =
                        this.provider()
                            .sealed_header_by_hash(parent_hash)
                            .map_err(|error| {
                                rpc_error(
                                    BLOCK_OR_HISTORY_UNAVAILABLE,
                                    "BLOCK_OR_HISTORY_UNAVAILABLE",
                                    error,
                                )
                            })?
                            .ok_or_else(|| {
                                rpc_error(
                                    BLOCK_OR_HISTORY_UNAVAILABLE,
                                    "BLOCK_OR_HISTORY_UNAVAILABLE",
                                    parent_hash,
                                )
                            })?;
                    if parent.number() != parent_number {
                        return Err(rpc_error(
                            EXECUTION_CONSENSUS_MISMATCH,
                            "EXECUTION_CONSENSUS_MISMATCH",
                            format!(
                                "parent header height {} differs from resolved state height \
                                 {parent_number}",
                                parent.number()
                            ),
                        ));
                    }
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::ProviderLoaded);
                    }
                    if verify_state_roots {
                        check_cancelled(&cancel, "before parent state root")?;
                        let provider_root = state_provider
                            .state_root(HashedPostState::default())
                            .map_err(|error| {
                                rpc_error(
                                    BLOCK_OR_HISTORY_UNAVAILABLE,
                                    "BLOCK_OR_HISTORY_UNAVAILABLE",
                                    error,
                                )
                            })?;
                        if provider_root != parent.state_root() {
                            return Err(rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                format!(
                                    "parent state provider root {provider_root} != parent header root {}",
                                    parent.state_root()
                                ),
                            ));
                        }
                    }
                    validate_block_hash_window(
                        &*state_provider,
                        block.number(),
                        parent_hash,
                    )?;
                    let db = StateProviderDatabase::new(&*state_provider);
                    let mut state = State::builder().with_database(db).with_bundle_update().build();

                    // `executor_for_block` installs a no-op inspector. Mirror its v1.3 assembly
                    // with the production block environment and execution context so the only
                    // difference is the DeBank tracing inspector.
                    let evm_config = this.evm_config();
                    let evm_env = evm_config.evm_env(block.header()).map_err(|error| {
                        rpc_error(
                            EXECUTION_CONSENSUS_MISMATCH,
                            "EXECUTION_CONSENSUS_MISMATCH",
                            error,
                        )
                    })?;
                    let ctx =
                        evm_config.context_for_block(block.sealed_block()).map_err(|error| {
                            rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                error,
                            )
                        })?;
                    let evm = evm_config.evm_with_env_and_inspector(
                        &mut state,
                        evm_env,
                        (
                            TracingInspector::new(debank_tracing_config()),
                            GethErrorInspector::default(),
                        ),
                    );
                    let mut executor = evm_config.create_executor(evm, ctx);
                    check_cancelled(&cancel, "before pre-execution")?;
                    executor.evm_mut().set_inspector_enabled(false);
                    executor.apply_pre_execution_changes().map_err(|error| {
                        rpc_error(
                            EXECUTION_CONSENSUS_MISMATCH,
                            "EXECUTION_CONSENSUS_MISMATCH",
                            error,
                        )
                    })?;
                    executor.evm_mut().set_inspector_enabled(true);
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::PreExecutionApplied);
                    }
                    check_cancelled(&cancel, "after pre-execution")?;

                    let mut trace_results = Vec::with_capacity(block.body().transaction_count());
                    let mut log_index = 0;
                    for transaction in block.transactions_recovered() {
                        check_cancelled(&cancel, "before transaction")?;
                        let tx_hash = *transaction.tx_hash();
                        let result = executor
                            .execute_transaction_without_commit(transaction)
                            .map_err(|error| {
                                rpc_error(
                                    EXECUTION_CONSENSUS_MISMATCH,
                                    "EXECUTION_CONSENSUS_MISMATCH",
                                    error,
                                )
                            })?;
                        #[cfg(test)]
                        if let Some(probe) = replay_test_probe.as_ref() {
                            probe.phase_reached(ReplayTestPhase::TransactionExecuted);
                        }
                        check_cancelled(&cancel, "after transaction execution")?;
                        let arena = executor.evm().inspector().0.traces().clone();
                        let exact_errors = executor
                            .evm_mut()
                            .inspector_mut()
                            .1
                            .finish_transaction(arena.nodes().len());
                        executor.evm_mut().inspector_mut().0.fuse();
                        let exact_errors = exact_errors.map_err(|error| {
                            rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                error,
                            )
                        })?;
                        #[cfg(test)]
                        if let Some(probe) = replay_test_probe.as_ref() {
                            probe.phase_reached(ReplayTestPhase::BeforeFormatter);
                        }
                        check_cancelled(&cancel, "before formatter")?;
                        trace_results.push(
                            build_debank_traces(
                                tx_hash,
                                arena,
                                &exact_errors,
                                &mut log_index,
                            )
                            .map_err(|error| {
                                rpc_error(
                                    EXECUTION_CONSENSUS_MISMATCH,
                                    "EXECUTION_CONSENSUS_MISMATCH",
                                    error,
                                )
                            })?,
                        );
                        #[cfg(test)]
                        if let Some(probe) = replay_test_probe.as_ref() {
                            probe.phase_reached(ReplayTestPhase::FormatterCompleted);
                        }
                        check_cancelled(&cancel, "after formatter")?;
                        executor.commit_transaction(result).map_err(|error| {
                            rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                error,
                            )
                        })?;
                        check_cancelled(&cancel, "after transaction commit")?;
                    }
                    executor.evm_mut().set_inspector_enabled(false);
                    let (_, execution) = executor.finish().map_err(|error| {
                        rpc_error(
                            EXECUTION_CONSENSUS_MISMATCH,
                            "EXECUTION_CONSENSUS_MISMATCH",
                            error,
                        )
                    })?;
                    let requests_match = if is_unzen_active {
                        block.header().requests_hash() ==
                            Some(execution.requests.requests_hash())
                    } else {
                        block.header().requests_hash().is_none()
                    };
                    if execution.receipts != *stored_receipts ||
                        execution.gas_used != block.header().gas_used() ||
                        !requests_match
                    {
                        return Err(rpc_error(
                            EXECUTION_CONSENSUS_MISMATCH,
                            "EXECUTION_CONSENSUS_MISMATCH",
                            "replayed receipts, gasUsed, or requests differ from the header",
                        ));
                    }

                    state.merge_transitions(BundleRetention::PlainState);
                    let bundle = state.take_bundle();
                    let replay_root = if verify_state_roots {
                        check_cancelled(&cancel, "before replay state root")?;
                        let hashed =
                            HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
                        let root = state_provider.state_root(hashed).map_err(|error| {
                            rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                error,
                            )
                        })?;
                        if root != block.state_root() {
                            return Err(rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                format!(
                                    "replay root {root} != header root {}",
                                    block.state_root()
                                ),
                            ));
                        }
                        #[cfg(test)]
                        if let Some(probe) = replay_test_probe.as_ref() {
                            probe.phase_reached(ReplayTestPhase::RootAComputed);
                        }
                        check_cancelled(&cancel, "after replay state root")?;
                        Some(root)
                    } else {
                        None
                    };
                    let state_diff = build_storage_diff(
                        &bundle,
                        block.state_root(),
                        parent.state_root(),
                    )
                        .map_err(|error| {
                            rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                error,
                            )
                    })?;
                    check_cancelled(&cancel, "before state-diff encoding")?;
                    // The legacy pipeline represents a state-neutral block with empty bytes,
                    // while non-empty diffs carry their parent/current root metadata in RLP.
                    let encoded = if block.state_root() == parent.state_root() {
                        Vec::new()
                    } else {
                        alloy_rlp::encode(&state_diff)
                    };
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::StateDiffEncoded);
                    }
                    check_cancelled(&cancel, "after state-diff encoding")?;
                    let decoded_state = if encoded.is_empty() {
                        HashedPostState::default()
                    } else {
                        decode_and_validate_storage_diff_exact(&encoded, &state_diff).map_err(
                            |error| {
                                rpc_error(
                                    EXECUTION_CONSENSUS_MISMATCH,
                                    "EXECUTION_CONSENSUS_MISMATCH",
                                    error,
                                )
                            },
                        )?
                    };
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::StateDiffDecoded);
                    }
                    if let Some(root_a) = replay_root {
                        check_cancelled(&cancel, "before decoded state root")?;
                        let root_b = state_provider.state_root(decoded_state).map_err(|error| {
                            rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                error,
                            )
                        })?;
                        if root_b != root_a {
                            return Err(rpc_error(
                                EXECUTION_CONSENSUS_MISMATCH,
                                "EXECUTION_CONSENSUS_MISMATCH",
                                format!("decoded root {root_b} != replay root {root_a}"),
                            ));
                        }
                        #[cfg(test)]
                        if let Some(probe) = replay_test_probe.as_ref() {
                            probe.phase_reached(ReplayTestPhase::RootBComputed);
                        }
                        check_cancelled(&cancel, "after decoded state root")?;
                    }

                    let mut block_file = BlockFile {
                        block: DebankBlock::from(block.as_ref()),
                        transactions: build_transactions(&block, &stored_receipts),
                        ..Default::default()
                    };
                    for (traces, error_traces, events, error_events) in trace_results {
                        block_file.traces.extend(traces);
                        block_file.error_traces.extend(error_traces);
                        block_file.events.extend(events);
                        block_file.error_events.extend(error_events);
                    }
                    validate_formatted_events(&block_file, &stored_receipts)?;
                    block_file.storage_contracts =
                        observed_storage_contracts(&block_file.traces, &block_file.error_traces);
                    let validation_hash = block_file.validation().validation_hash;
                    let output = DebankOutput {
                        block_file,
                        header: build_rpc_header(&block),
                        state_diff: encoded.into(),
                        validation_hash,
                    };
                    #[cfg(test)]
                    let output_resource = replay_test_probe
                        .as_ref()
                        .and_then(|probe| probe.resource_guard(ReplayTestResource::OutputBuffer));
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::OutputBuilt);
                    }
                    check_cancelled(&cancel, "before JSON serialization")?;
                    let serialized = serde_json::value::to_raw_value(&output).map_err(|error| {
                        rpc_error(
                            EXECUTION_CONSENSUS_MISMATCH,
                            "EXECUTION_CONSENSUS_MISMATCH",
                            error,
                        )
                    })?;
                    #[cfg(test)]
                    let serialized_resource = replay_test_probe
                        .as_ref()
                        .and_then(|probe| probe.resource_guard(ReplayTestResource::SerializedBuffer));
                    drop(output);
                    #[cfg(test)]
                    drop(output_resource);
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::JsonSerializationCompleted);
                    }
                    check_cancelled(&cancel, "after JSON serialization")?;
                    #[cfg(test)]
                    if let Some(probe) = replay_test_probe.as_ref() {
                        probe.phase_reached(ReplayTestPhase::JsonSerialized);
                    }
                    Ok(ReplayResponse {
                        serialized,
                        _permit: permit,
                        #[cfg(test)]
                        _serialized_resource: serialized_resource,
                    })
                })())
            })
            .await
            .map_err(|error| {
                rpc_error(BLOCK_OR_HISTORY_UNAVAILABLE, "BLOCK_OR_HISTORY_UNAVAILABLE", error)
            })?
    }
}

/// Verifies body and stored receipt commitments before replay uses them as truth.
fn validate_stored_consensus(
    block: &RecoveredBlock<reth_ethereum_primitives::Block>,
    receipts: &[reth_ethereum_primitives::Receipt],
    is_unzen_active: bool,
) -> RpcResult<()> {
    let transactions = BlockBody::transactions(block.body());
    let tx_root = alloy_consensus::proofs::calculate_transaction_root(transactions);
    let receipt_root = reth_ethereum_primitives::calculate_receipt_root_no_memo(receipts);
    let receipt_bloom = logs_bloom(receipts.iter().flat_map(|receipt| receipt.logs()));
    let cumulative_gas = receipts.last().map_or(0, TxReceipt::cumulative_gas_used);
    let ommers_are_empty =
        block.body().ommers.is_empty() && block.header().ommers_hash() == EMPTY_OMMER_ROOT_HASH;
    let withdrawals_match =
        block.body().calculate_withdrawals_root() == block.header().withdrawals_root();
    let withdrawals_are_present = !is_unzen_active ||
        (block.body().withdrawals.is_some() && block.header().withdrawals_root().is_some());
    let fork_fields_are_valid = if is_unzen_active {
        block.header().requests_hash() == Some(EMPTY_REQUESTS_HASH) &&
            block.header().parent_beacon_block_root() == Some(alloy_primitives::B256::ZERO) &&
            block.header().blob_gas_used() == Some(0) &&
            block.header().excess_blob_gas() == Some(0)
    } else {
        block.header().requests_hash().is_none() &&
            block.header().parent_beacon_block_root().is_none() &&
            block.header().blob_gas_used().is_none() &&
            block.header().excess_blob_gas().is_none() &&
            block.header().difficulty().is_zero()
    };
    let protocol_fields_are_valid = fork_fields_are_valid &&
        block.header().block_access_list_hash().is_none() &&
        block.header().slot_number().is_none();
    if block.body().transaction_count() != receipts.len() ||
        tx_root != block.transactions_root() ||
        receipt_root != block.receipts_root() ||
        receipt_bloom != block.logs_bloom() ||
        cumulative_gas != block.gas_used() ||
        !ommers_are_empty ||
        !withdrawals_match ||
        !withdrawals_are_present ||
        !protocol_fields_are_valid
    {
        return Err(rpc_error(
            EXECUTION_CONSENSUS_MISMATCH,
            "EXECUTION_CONSENSUS_MISMATCH",
            "stored body/receipts or protocol commitments do not match the header",
        ));
    }
    Ok(())
}

/// Returns whether a transaction is unsupported by Taiko's canonical execution path.
fn is_unsupported_debank_transaction(transaction: &impl Transaction) -> bool {
    transaction.is_eip4844()
}

/// Verifies the formatter's surviving log projection against stored receipt logs.
fn validate_formatted_events(
    block_file: &BlockFile,
    receipts: &[reth_ethereum_primitives::Receipt],
) -> RpcResult<()> {
    let stored_logs = receipts.iter().flat_map(|receipt| receipt.logs()).collect::<Vec<_>>();
    if block_file.events.len() != stored_logs.len() {
        return Err(rpc_error(
            EXECUTION_CONSENSUS_MISMATCH,
            "EXECUTION_CONSENSUS_MISMATCH",
            format!(
                "formatted event count {} != stored log count {}",
                block_file.events.len(),
                stored_logs.len()
            ),
        ));
    }
    let mut seen = vec![false; stored_logs.len()];
    for event in &block_file.events {
        let index = event.idx;
        let Some(log) = stored_logs.get(index) else {
            return Err(rpc_error(
                EXECUTION_CONSENSUS_MISMATCH,
                "EXECUTION_CONSENSUS_MISMATCH",
                format!("formatted event idx {index} is outside stored receipt logs"),
            ));
        };
        if std::mem::replace(&mut seen[index], true) {
            return Err(rpc_error(
                EXECUTION_CONSENSUS_MISMATCH,
                "EXECUTION_CONSENSUS_MISMATCH",
                format!("formatted event idx {index} is duplicated"),
            ));
        }
        let topics = log.topics();
        let selector = topics.first().map(ToString::to_string).unwrap_or_default();
        let remaining = (!topics.is_empty())
            .then(|| topics[1..].iter().map(ToString::to_string).collect::<Vec<_>>());
        if event.contract_id != log.address ||
            event.selector != selector ||
            event.topics != remaining ||
            event.data != log.data.data
        {
            return Err(rpc_error(
                EXECUTION_CONSENSUS_MISMATCH,
                "EXECUTION_CONSENSUS_MISMATCH",
                format!("formatted event idx {index} differs from stored receipt log"),
            ));
        }
    }
    Ok(())
}

/// Stops before starting a new replay stage after deadline or client disconnect.
fn check_cancelled(cancel: &CancelOnDrop, stage: &'static str) -> RpcResult<()> {
    if cancel.is_cancelled() {
        return Err(rpc_error(REQUEST_CANCELLED, "REQUEST_CANCELLED", stage));
    }
    Ok(())
}

/// Builds block-file transactions from exact stored primitive receipts.
fn build_transactions(
    block: &RecoveredBlock<reth_ethereum_primitives::Block>,
    receipts: &[reth_ethereum_primitives::Receipt],
) -> Vec<DebankTransaction> {
    let mut prior_cumulative_gas = 0;
    block
        .transactions_recovered()
        .zip(receipts)
        .enumerate()
        .map(|(index, (transaction, receipt))| {
            let cumulative_gas = receipt.cumulative_gas_used();
            let gas_used = cumulative_gas.saturating_sub(prior_cumulative_gas);
            prior_cumulative_gas = cumulative_gas;
            let (gas_fee_cap, gas_tip_cap) = blockfile_fee_caps(*transaction.inner());
            DebankTransaction {
                id: transaction.tx_hash().to_string(),
                from: transaction.signer(),
                to: blockfile_transaction_to(
                    transaction.kind(),
                    transaction.signer(),
                    transaction.nonce(),
                ),
                gas_limit: transaction.gas_limit(),
                gas_price: transaction.effective_gas_price(block.base_fee_per_gas()),
                gas_used,
                status: receipt.status(),
                gas_fee_cap,
                gas_tip_cap,
                input: transaction.input().clone(),
                nonce: transaction.nonce(),
                transaction_index: index as u64,
                value: transaction.value(),
            }
        })
        .collect()
}

/// Returns the block-file fee cap fields used by the legacy pipeline.
fn blockfile_fee_caps(transaction: &impl Transaction) -> (u128, u128) {
    transaction
        .max_priority_fee_per_gas()
        .map_or((0, 0), |tip| (transaction.max_fee_per_gas(), tip))
}

/// Returns the historical block-file recipient, including the derived CREATE address.
fn blockfile_transaction_to(kind: TxKind, signer: Address, nonce: u64) -> Address {
    kind.to().copied().unwrap_or_else(|| signer.create(nonce))
}

/// Returns the minimal inspector configuration needed by the historical block-file wire.
fn debank_tracing_config() -> TracingInspectorConfig {
    let mut config = TracingInspectorConfig::default_parity()
        .set_steps(true)
        .set_record_logs(true)
        .set_exclude_precompile_calls(false);
    config.record_opcodes_filter =
        Some(OpcodeFilter::new().enabled(reth_revm::bytecode::opcode::OpCode::SSTORE));
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof_state::ProofHistoryReadiness;
    use alethia_reth_chainspec::{TAIKO_MAINNET, spec::TaikoChainSpec};
    use alloy_consensus::{
        Block, BlockBody as AlloyBlockBody, Header, Signed, TxEip1559, TxEip2930, TxEip4844,
        TxEip7702, TxLegacy, TxType,
    };
    use alloy_eips::eip4895::{Withdrawal, Withdrawals};
    use alloy_primitives::{B256, Bytes, ChainId, Signature, U256};
    use futures_util::stream;
    use jsonrpsee::{
        core::client::{ClientT, Error as ClientError},
        http_client::HttpClientBuilder,
        rpc_params,
        server::{ServerBuilder, ServerConfig},
    };
    use reth::{
        network::noop::NoopNetwork,
        tasks::{Runtime, pool::BlockingTaskPool},
    };
    use reth_chain_state::CanonStateNotification;
    use reth_execution_types::{Chain, ExecutionOutcome};
    use reth_optimism_trie::InMemoryProofsStorage;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_rpc::EthApiBuilder;
    use reth_rpc_eth_types::{EthStateCache, cache::cache_new_blocks_task};
    use reth_transaction_pool::test_utils::testing_pool;
    use socket2::SockRef;
    use std::{
        collections::HashSet,
        sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpSocket,
    };

    fn empty_replay_block(
        number: u64,
        parent_hash: B256,
    ) -> RecoveredBlock<reth_ethereum_primitives::Block> {
        RecoveredBlock::new_unhashed(
            reth_ethereum_primitives::Block {
                header: Header {
                    parent_hash,
                    ommers_hash: EMPTY_OMMER_ROOT_HASH,
                    state_root: B256::ZERO,
                    transactions_root: EMPTY_TRANSACTIONS,
                    receipts_root: EMPTY_RECEIPTS,
                    withdrawals_root: Some(EMPTY_WITHDRAWALS),
                    logs_bloom: Bloom::ZERO,
                    difficulty: U256::ZERO,
                    number,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: number + 1,
                    base_fee_per_gas: Some(1),
                    ..Default::default()
                },
                body: reth_ethereum_primitives::BlockBody {
                    withdrawals: Some(Default::default()),
                    ..Default::default()
                },
            },
            vec![],
        )
    }

    fn single_transaction_replay_block(
        number: u64,
        parent_hash: B256,
    ) -> (RecoveredBlock<reth_ethereum_primitives::Block>, reth_ethereum_primitives::Receipt, Address)
    {
        let sender = Address::repeat_byte(0x11);
        let transaction = Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(ChainId::from(167_000_u64)),
                nonce: 0,
                gas_price: 1,
                gas_limit: 21_000,
                to: TxKind::Call(Address::repeat_byte(0x22)),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            Signature::new(U256::from(1), U256::from(2), false),
            B256::repeat_byte(0x33),
        )
        .into();
        let receipt = reth_ethereum_primitives::Receipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 21_000,
            logs: vec![],
        };
        let transactions = vec![transaction];
        let receipts = [receipt.clone()];
        let block = RecoveredBlock::new_unhashed(
            reth_ethereum_primitives::Block {
                header: Header {
                    parent_hash,
                    ommers_hash: EMPTY_OMMER_ROOT_HASH,
                    state_root: B256::ZERO,
                    transactions_root: alloy_consensus::proofs::calculate_transaction_root(
                        &transactions,
                    ),
                    receipts_root: reth_ethereum_primitives::calculate_receipt_root_no_memo(
                        &receipts,
                    ),
                    withdrawals_root: Some(EMPTY_WITHDRAWALS),
                    logs_bloom: Bloom::ZERO,
                    difficulty: U256::ZERO,
                    number,
                    gas_limit: 30_000_000,
                    gas_used: 21_000,
                    timestamp: number + 1,
                    base_fee_per_gas: Some(1),
                    ..Default::default()
                },
                body: reth_ethereum_primitives::BlockBody {
                    transactions,
                    withdrawals: Some(Default::default()),
                    ..Default::default()
                },
            },
            vec![sender],
        );
        (block, receipt, sender)
    }

    fn genesis_replay_block(
        chain_spec: &TaikoChainSpec,
    ) -> RecoveredBlock<reth_ethereum_primitives::Block> {
        RecoveredBlock::new_unhashed(
            reth_ethereum_primitives::Block {
                header: chain_spec.genesis_header().clone(),
                body: reth_ethereum_primitives::BlockBody {
                    withdrawals: Some(Default::default()),
                    ..Default::default()
                },
            },
            vec![],
        )
    }

    async fn cache_replay_blocks(
        provider: &MockEthProvider<EthPrimitives, TaikoChainSpec>,
        blocks: Vec<RecoveredBlock<reth_ethereum_primitives::Block>>,
    ) -> EthStateCache<EthPrimitives> {
        let cache =
            EthStateCache::spawn_with(provider.clone(), Default::default(), Runtime::test());
        let first_block = blocks.first().unwrap().number();
        let block_count = blocks.len();
        let receipts = blocks
            .iter()
            .map(|block| provider.receipts.lock().get(&block.number()).cloned().unwrap_or_default())
            .collect();
        let outcome = ExecutionOutcome::new(
            Default::default(),
            receipts,
            first_block,
            vec![Default::default(); block_count],
        );
        let notification = CanonStateNotification::Commit {
            new: Arc::new(Chain::new(blocks, outcome, Default::default())),
        };
        cache_new_blocks_task(cache.clone(), stream::iter([notification])).await;
        cache
    }

    async fn assert_replay_resources_drained(resources: &ReplayTestResources, context: &str) {
        let drained = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if resources.snapshot() == ReplayTestResourceSnapshot::default() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "{context} retained replay resources after 60 seconds: {:?}",
            resources.snapshot()
        );
    }

    fn assert_phase_resource_snapshot(resources: &ReplayTestResources, phase: ReplayTestPhase) {
        let snapshot = resources.snapshot();
        assert_eq!(snapshot.workers, 1, "phase {phase:?} should retain one worker");
        assert_eq!(snapshot.permits, 1, "phase {phase:?} should retain one permit");
        assert_eq!(
            snapshot.provider_readers, 1,
            "phase {phase:?} should retain one provider reader"
        );
        assert_eq!(
            snapshot.output_buffers,
            usize::from(phase == ReplayTestPhase::OutputBuilt),
            "phase {phase:?} output buffer count"
        );
        assert_eq!(
            snapshot.serialized_buffers,
            usize::from(phase == ReplayTestPhase::JsonSerializationCompleted),
            "phase {phase:?} serialized buffer count"
        );
    }

    #[tokio::test]
    async fn replay_guard_allows_configured_parallelism() {
        let tracer = DebankTraceExt::<(), ()>::new((), None, 2);
        let first =
            tracer.acquire_replay_permit(Instant::now() + Duration::from_secs(1)).await.unwrap();
        let second =
            tracer.acquire_replay_permit(Instant::now() + Duration::from_secs(1)).await.unwrap();

        let blocked =
            tracer.acquire_replay_permit(Instant::now() + Duration::from_millis(20)).await;
        let blocked = blocked.unwrap_err();
        assert_eq!(blocked.code(), REQUEST_CANCELLED);
        assert_eq!(blocked.message(), "REQUEST_CANCELLED");

        drop(first);
        assert!(
            tracer.acquire_replay_permit(Instant::now() + Duration::from_secs(1)).await.is_ok()
        );
        drop(second);
    }

    #[tokio::test]
    async fn root_disabled_replay_uses_archive_outside_proof_history() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        let parent = empty_replay_block(0, B256::ZERO);
        let target = empty_replay_block(1, parent.hash());
        let distant_tip = empty_replay_block(2_000, B256::repeat_byte(0x20));
        for block in [&parent, &target, &distant_tip] {
            provider.add_block(block.hash(), block.clone().into_block());
            provider.add_receipts(block.number(), vec![]);
        }
        provider.state_roots.lock().extend([B256::repeat_byte(0xee); 3]);
        let cache = cache_replay_blocks(&provider, vec![target]).await;
        let eth = EthApiBuilder::new(
            provider.clone(),
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();
        let storage: reth_optimism_trie::OpProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::new().into();
        let proof_history = ProofHistoryStateProviderFactory::new(
            eth.clone(),
            storage,
            ProofHistoryReadiness::new(),
        );
        let module = DebankTraceExt::new(eth, Some(proof_history), 1).into_rpc();
        let block_id = serde_json::to_string(&BlockId::number(1)).unwrap();
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"trace_debankBlock","params":[{block_id}]}}"#
        );

        let (raw_response, _) =
            tokio::time::timeout(Duration::from_secs(10), module.raw_json_request(&request, 1))
                .await
                .unwrap()
                .unwrap();
        let response: serde_json::Value = serde_json::from_str(raw_response.get()).unwrap();
        assert_eq!(response["result"]["state_diff"], "0x", "{response:#}");
        assert!(response.get("error").is_none());
        assert_eq!(
            provider.state_roots.lock().len(),
            3,
            "state-root verification must be disabled by default"
        );
    }

    #[tokio::test]
    async fn missing_block_hash_window_is_rejected_before_pre_execution() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        let parent = empty_replay_block(256, B256::repeat_byte(0x25));
        let target = empty_replay_block(257, parent.hash());
        for block in [&parent, &target] {
            provider.add_block(block.hash(), block.clone().into_block());
            provider.add_receipts(block.number(), vec![]);
        }
        let cache = cache_replay_blocks(&provider, vec![target]).await;
        let eth = EthApiBuilder::new(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();
        let pre_execution_reached = Arc::new(AtomicBool::new(false));
        let callback_reached = pre_execution_reached.clone();
        let probe = ReplayTestProbe::for_phase(move |phase| {
            if phase == ReplayTestPhase::PreExecutionApplied {
                callback_reached.store(true, Ordering::SeqCst);
            }
        });
        let module = DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1)
            .with_replay_test_probe(probe)
            .into_rpc();
        let block_id = serde_json::to_string(&BlockId::number(257)).unwrap();
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"trace_debankBlock","params":[{block_id}]}}"#
        );

        let (raw_response, _) =
            tokio::time::timeout(Duration::from_secs(10), module.raw_json_request(&request, 1))
                .await
                .unwrap()
                .unwrap();
        let response: serde_json::Value = serde_json::from_str(raw_response.get()).unwrap();
        assert_eq!(response["error"]["code"], BLOCK_OR_HISTORY_UNAVAILABLE);
        assert_eq!(response["error"]["message"], "BLOCK_OR_HISTORY_UNAVAILABLE");
        assert!(
            response["error"]["data"].as_str().unwrap().contains("requires 256 canonical hashes"),
            "{response:#}"
        );
        assert!(response.get("result").is_none());
        assert!(
            !pre_execution_reached.load(Ordering::SeqCst),
            "missing BLOCKHASH history must stop before pre-execution"
        );
    }

    async fn assert_reorg_error_at_phase(phase: ReplayTestPhase, replacement_hash: Option<B256>) {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        // Mock state roots are sufficient for this fault-control test only. The MDBX integration
        // test proves state-root correctness with a real trie.
        let parent = empty_replay_block(0, B256::ZERO);
        let (target, receipts) = if phase == ReplayTestPhase::TransactionExecuted {
            let (target, receipt, sender) = single_transaction_replay_block(1, parent.hash());
            provider.add_account(sender, ExtendedAccount::new(0, U256::from(1_000_000_u64)));
            (target, vec![receipt])
        } else {
            (empty_replay_block(1, parent.hash()), vec![])
        };

        for block in [&parent, &target] {
            provider.add_block(block.hash(), block.clone().into_block());
        }
        provider.add_receipts(parent.number(), vec![]);
        provider.add_receipts(target.number(), receipts);
        let cache = cache_replay_blocks(&provider, vec![target.clone()]).await;
        let eth = EthApiBuilder::new(
            provider.clone(),
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();

        let original_hash = target.hash();
        if let Some(replacement_hash) = replacement_hash {
            assert_ne!(original_hash, replacement_hash);
        }
        let alternate_header = target.header().clone();
        let reorg_provider = provider.clone();
        let injected = Arc::new(AtomicBool::new(false));
        let callback_injected = injected.clone();
        let probe = ReplayTestProbe::for_phase(move |reached| {
            if reached == phase && !callback_injected.swap(true, Ordering::SeqCst) {
                reorg_provider.headers.lock().remove(&original_hash);
                if let Some(replacement_hash) = replacement_hash {
                    reorg_provider.add_header(replacement_hash, alternate_header.clone());
                }
            }
        });
        let module = DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1)
            .with_replay_test_probe(probe)
            .into_rpc();
        let block_id = serde_json::to_string(&BlockId::number(1)).unwrap();
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"trace_debankBlock","params":[{block_id}]}}"#
        );
        let (raw_response, _) =
            tokio::time::timeout(Duration::from_secs(10), module.raw_json_request(&request, 1))
                .await
                .unwrap()
                .unwrap();

        assert!(injected.load(Ordering::SeqCst), "phase {phase:?} was not reached");
        assert_eq!(provider.block_hash(1).unwrap(), replacement_hash);
        let response: serde_json::Value = serde_json::from_str(raw_response.get()).unwrap();
        assert_eq!(
            response.as_object().unwrap().len(),
            3,
            "reorg response must contain only jsonrpc, id, and error"
        );
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["error"]["code"], BLOCK_REORGED);
        assert_eq!(response["error"]["message"], "BLOCK_REORGED");
        assert!(
            response.get("result").is_none(),
            "reorg response must not include serialized block output"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reorg_after_initial_canonical_check_returns_only_error() {
        assert_reorg_error_at_phase(
            ReplayTestPhase::InitialCanonicalChecked,
            Some(B256::repeat_byte(0xfa)),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reorg_during_transaction_replay_returns_only_error() {
        assert_reorg_error_at_phase(
            ReplayTestPhase::TransactionExecuted,
            Some(B256::repeat_byte(0xfa)),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reorg_after_json_serialization_returns_only_error() {
        assert_reorg_error_at_phase(ReplayTestPhase::JsonSerialized, Some(B256::repeat_byte(0xfa)))
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_head_rollback_during_replay_returns_only_reorg_error() {
        assert_reorg_error_at_phase(ReplayTestPhase::TransactionExecuted, None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proof_history_parent_resolution_error_prefers_target_reorg() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        let grandparent = empty_replay_block(0, B256::ZERO);
        let parent = empty_replay_block(1, grandparent.hash());
        let target = empty_replay_block(2, parent.hash());
        for block in [&grandparent, &parent, &target] {
            provider.add_block(block.hash(), block.clone().into_block());
            provider.add_receipts(block.number(), vec![]);
        }
        let cache = cache_replay_blocks(&provider, vec![target.clone()]).await;
        let eth = EthApiBuilder::new(
            provider.clone(),
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();

        let original_parent_hash = parent.hash();
        let original_target_hash = target.hash();
        let replacement_parent_hash = B256::repeat_byte(0xfb);
        let replacement_target_hash = B256::repeat_byte(0xfc);
        let replacement_parent_header = parent.header().clone();
        let mut replacement_target_header = target.header().clone();
        replacement_target_header.parent_hash = replacement_parent_hash;
        let reorg_provider = provider.clone();
        let injected = Arc::new(AtomicBool::new(false));
        let callback_injected = injected.clone();
        let provider_loaded = Arc::new(AtomicBool::new(false));
        let callback_provider_loaded = provider_loaded.clone();
        let resources = ReplayTestResources::default();
        let probe = ReplayTestProbe::for_phase(move |phase| {
            if phase == ReplayTestPhase::InitialCanonicalChecked &&
                !callback_injected.swap(true, Ordering::SeqCst)
            {
                let mut headers = reorg_provider.headers.lock();
                headers.remove(&original_parent_hash);
                headers.remove(&original_target_hash);
                headers.insert(replacement_parent_hash, replacement_parent_header.clone());
                headers.insert(replacement_target_hash, replacement_target_header.clone());
            }
            if phase == ReplayTestPhase::ProviderLoaded {
                callback_provider_loaded.store(true, Ordering::SeqCst);
            }
        })
        .with_resources(resources.clone());
        let storage: reth_optimism_trie::OpProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::new().into();
        let proof_history = ProofHistoryStateProviderFactory::new(
            eth.clone(),
            storage,
            ProofHistoryReadiness::new(),
        );
        let module = DebankTraceExt::new(eth, Some(proof_history), 1)
            .with_replay_test_probe(probe)
            .into_rpc();
        let block_id = serde_json::to_string(&BlockId::number(2)).unwrap();
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"trace_debankBlock","params":[{block_id}]}}"#
        );
        let (raw_response, _) =
            tokio::time::timeout(Duration::from_secs(10), module.raw_json_request(&request, 1))
                .await
                .unwrap()
                .unwrap();

        assert!(injected.load(Ordering::SeqCst));
        assert!(!provider_loaded.load(Ordering::SeqCst));
        assert!(!provider.headers.lock().contains_key(&original_parent_hash));
        assert_eq!(provider.block_hash(1).unwrap(), Some(replacement_parent_hash));
        assert_eq!(provider.block_hash(2).unwrap(), Some(replacement_target_hash));
        let response: serde_json::Value = serde_json::from_str(raw_response.get()).unwrap();
        assert_eq!(response.as_object().unwrap().len(), 3);
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["error"]["code"], BLOCK_REORGED);
        assert_eq!(response["error"]["message"], "BLOCK_REORGED");
        assert!(response.get("result").is_none());
        let detail = response["error"]["data"].as_str().unwrap();
        assert!(detail.contains("height 2"), "{detail}");
        assert!(detail.contains(&original_target_hash.to_string()), "{detail}");
        assert!(detail.contains(&replacement_target_hash.to_string()), "{detail}");
        assert_replay_resources_drained(&resources, "proof-history parent resolve reorg").await;
    }

    async fn assert_request_drop_stops_after_phase(
        blocked_phase: ReplayTestPhase,
        forbidden_phase: ReplayTestPhase,
    ) {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        // Mock roots are sufficient for cancellation control flow only. State-root correctness is
        // proved separately by the MDBX integration test with a real trie.
        let parent = empty_replay_block(0, B256::ZERO);
        let (target, receipt, sender) = single_transaction_replay_block(1, parent.hash());
        provider.add_account(sender, ExtendedAccount::new(0, U256::from(1_000_000_u64)));
        for block in [&parent, &target] {
            provider.add_block(block.hash(), block.clone().into_block());
        }
        provider.add_receipts(parent.number(), vec![]);
        provider.add_receipts(target.number(), vec![receipt]);
        let cache = cache_replay_blocks(&provider, vec![target]).await;
        let eth = EthApiBuilder::new(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = released.clone();
        let forbidden = Arc::new(AtomicBool::new(false));
        let worker_forbidden = forbidden.clone();
        let resources = ReplayTestResources::default();
        let probe = ReplayTestProbe::for_phase(move |phase| {
            if phase == forbidden_phase {
                worker_forbidden.store(true, Ordering::SeqCst);
            }
            if phase == blocked_phase {
                entered_tx.send(()).unwrap();
                let (lock, wake) = &*worker_release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    let (next, wait) =
                        wake.wait_timeout(released, Duration::from_secs(10)).unwrap();
                    assert!(!wait.timed_out(), "timed out waiting to release replay worker");
                    released = next;
                }
            }
        })
        .with_resources(resources.clone());
        let tracer = Arc::new(
            DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1)
                .with_state_root_verification(true)
                .with_replay_test_probe(probe),
        );
        let request = {
            let tracer = tracer.clone();
            tokio::spawn(async move { tracer.debank_block(BlockId::number(1)).await })
        };
        tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("phase {blocked_phase:?} was not reached"))
            .unwrap_or_else(|| panic!("phase probe closed before reaching {blocked_phase:?}"));
        assert_phase_resource_snapshot(&resources, blocked_phase);

        request.abort();
        let join_error = request.await.expect_err("aborted request task must not complete");
        assert!(join_error.is_cancelled(), "request task should be cancelled");

        let (lock, wake) = &*released;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        let permit = tokio::time::timeout(
            Duration::from_secs(10),
            tracer.acquire_replay_permit(Instant::now() + Duration::from_secs(10)),
        )
        .await
        .unwrap_or_else(|_| panic!("phase {blocked_phase:?} did not release its replay permit"))
        .expect("replay permit should be reusable after cancellation");
        drop(permit);
        assert_replay_resources_drained(&resources, &format!("phase {blocked_phase:?}")).await;
        assert!(
            !forbidden.load(Ordering::SeqCst),
            "cancelled phase {blocked_phase:?} must not enter {forbidden_phase:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_drop_cancels_every_replay_stage_and_releases_permit() {
        let phase_matrix = [
            (ReplayTestPhase::ProviderLoaded, ReplayTestPhase::PreExecutionApplied),
            (ReplayTestPhase::PreExecutionApplied, ReplayTestPhase::TransactionExecuted),
            (ReplayTestPhase::TransactionExecuted, ReplayTestPhase::BeforeFormatter),
            (ReplayTestPhase::FormatterCompleted, ReplayTestPhase::RootAComputed),
            (ReplayTestPhase::RootAComputed, ReplayTestPhase::StateDiffEncoded),
            (ReplayTestPhase::StateDiffEncoded, ReplayTestPhase::StateDiffDecoded),
            (ReplayTestPhase::StateDiffDecoded, ReplayTestPhase::RootBComputed),
            (ReplayTestPhase::RootBComputed, ReplayTestPhase::OutputBuilt),
            (ReplayTestPhase::OutputBuilt, ReplayTestPhase::JsonSerializationCompleted),
            (ReplayTestPhase::JsonSerializationCompleted, ReplayTestPhase::JsonSerialized),
        ];

        for (blocked_phase, forbidden_phase) in phase_matrix {
            assert_request_drop_stops_after_phase(blocked_phase, forbidden_phase).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_deadline_cancels_before_formatter_and_releases_permit() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        // Mock roots are sufficient for cancellation control flow. Root correctness uses MDBX.
        let parent = empty_replay_block(0, B256::ZERO);
        let (target, receipt, sender) = single_transaction_replay_block(1, parent.hash());
        provider.add_account(sender, ExtendedAccount::new(0, U256::from(1_000_000_u64)));
        for block in [&parent, &target] {
            provider.add_block(block.hash(), block.clone().into_block());
        }
        provider.add_receipts(parent.number(), vec![]);
        provider.add_receipts(target.number(), vec![receipt]);
        let cache = cache_replay_blocks(&provider, vec![target]).await;
        let eth = EthApiBuilder::new(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = released.clone();
        let before_formatter = Arc::new(AtomicBool::new(false));
        let worker_before_formatter = before_formatter.clone();
        let resources = ReplayTestResources::default();
        let probe = ReplayTestProbe::for_phase(move |phase| match phase {
            ReplayTestPhase::TransactionExecuted => {
                entered_tx.send(()).unwrap();
                let (lock, wake) = &*worker_release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    let (next, wait) =
                        wake.wait_timeout(released, Duration::from_secs(10)).unwrap();
                    assert!(!wait.timed_out(), "timed out waiting to release replay worker");
                    released = next;
                }
            }
            ReplayTestPhase::BeforeFormatter => {
                worker_before_formatter.store(true, Ordering::SeqCst);
            }
            _ => {}
        })
        .with_resources(resources.clone());
        let tracer = Arc::new(
            DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1)
                .with_replay_test_probe(probe)
                .with_request_timeout(Duration::from_secs(2)),
        );
        let request = {
            let tracer = tracer.clone();
            tokio::spawn(async move { tracer.debank_block(BlockId::number(1)).await })
        };
        tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .unwrap()
            .expect("transaction phase should be reached");
        assert_phase_resource_snapshot(&resources, ReplayTestPhase::TransactionExecuted);

        let error = tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .expect("request should reach its deadline")
            .expect("request task should not panic")
            .expect_err("deadline must reject the request");
        assert_eq!(error.code(), REQUEST_CANCELLED);
        assert_eq!(error.message(), "REQUEST_CANCELLED");
        assert_phase_resource_snapshot(&resources, ReplayTestPhase::TransactionExecuted);

        let (lock, wake) = &*released;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        let permit = tokio::time::timeout(
            Duration::from_secs(10),
            tracer.acquire_replay_permit(Instant::now() + Duration::from_secs(10)),
        )
        .await
        .expect("cancelled replay worker should release its permit")
        .expect("replay permit should be reusable");
        drop(permit);
        assert_replay_resources_drained(&resources, "deadline cancellation").await;
        assert!(
            !before_formatter.load(Ordering::SeqCst),
            "cancelled worker must not start the formatter stage"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn http_disconnect_cancels_before_formatter_and_releases_permit() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        // Mock roots are sufficient for cancellation control flow. Root correctness uses MDBX.
        let parent = empty_replay_block(0, B256::ZERO);
        let (target, receipt, sender) = single_transaction_replay_block(1, parent.hash());
        provider.add_account(sender, ExtendedAccount::new(0, U256::from(1_000_000_u64)));
        for block in [&parent, &target] {
            provider.add_block(block.hash(), block.clone().into_block());
        }
        provider.add_receipts(parent.number(), vec![]);
        provider.add_receipts(target.number(), vec![receipt]);
        let cache = cache_replay_blocks(&provider, vec![target]).await;
        let eth = EthApiBuilder::new(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = released.clone();
        let before_formatter = Arc::new(AtomicBool::new(false));
        let worker_before_formatter = before_formatter.clone();
        let resources = ReplayTestResources::default();
        let probe = ReplayTestProbe::for_phase(move |phase| match phase {
            ReplayTestPhase::TransactionExecuted => {
                entered_tx.send(()).unwrap();
                let (lock, wake) = &*worker_release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    let (next, wait) =
                        wake.wait_timeout(released, Duration::from_secs(10)).unwrap();
                    assert!(!wait.timed_out(), "timed out waiting to release replay worker");
                    released = next;
                }
            }
            ReplayTestPhase::BeforeFormatter => {
                worker_before_formatter.store(true, Ordering::SeqCst);
            }
            _ => {}
        })
        .with_resources(resources.clone());
        let tracer = DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1)
            .with_replay_test_probe(probe);
        let replay_guard = tracer.replay_guard.clone();
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let server_handle = server.start(tracer.into_rpc());

        let socket = TcpSocket::new_v4().unwrap();
        let mut client = socket.connect(server_addr).await.unwrap();
        let block_id = serde_json::to_string(&BlockId::number(1)).unwrap();
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"trace_debankBlock","params":[{block_id}]}}"#
        );
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {server_addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len()
        );
        client.write_all(request.as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .unwrap()
            .expect("transaction phase should be reached");
        assert_phase_resource_snapshot(&resources, ReplayTestPhase::TransactionExecuted);

        SockRef::from(&client).set_linger(Some(Duration::ZERO)).unwrap();
        drop(client);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_phase_resource_snapshot(&resources, ReplayTestPhase::TransactionExecuted);
        let (lock, wake) = &*released;
        *lock.lock().unwrap() = true;
        wake.notify_all();

        let permit = tokio::time::timeout(Duration::from_secs(10), replay_guard.acquire_owned())
            .await
            .expect("disconnected replay worker should release its permit")
            .expect("replay permit should be reusable");
        drop(permit);
        assert_replay_resources_drained(&resources, "HTTP disconnect").await;
        assert!(
            !before_formatter.load(Ordering::SeqCst),
            "disconnected worker must not start the formatter stage"
        );

        server_handle.stop().unwrap();
        server_handle.stopped().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_debank_requests_overlap_inside_replay_workers() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        // The mock provider's zero roots are only fixtures for exercising replay concurrency.
        // State-root correctness is covered separately with a real MDBX/trie provider.
        let parent = empty_replay_block(0, B256::ZERO);
        let first = empty_replay_block(1, parent.hash());
        let second = empty_replay_block(2, first.hash());

        for block in [&parent, &first, &second] {
            provider.add_block(block.hash(), block.clone().into_block());
            provider.add_receipts(block.number(), vec![]);
        }
        let cache = cache_replay_blocks(&provider, vec![first, second]).await;
        let blocking_pool =
            BlockingTaskPool::new(BlockingTaskPool::builder().num_threads(2).build().unwrap());
        let eth = EthApiBuilder::new(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(blocking_pool)
        .build();

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let released = Arc::new((Mutex::new(HashSet::new()), Condvar::new()));
        let worker_releases = released.clone();
        let probe = ReplayTestProbe::new(move |block_number| {
            entered_tx.send(block_number).unwrap();
            let (lock, wake) = &*worker_releases;
            let mut released = lock.lock().unwrap();
            while !released.remove(&block_number) {
                let (next, wait) = wake.wait_timeout(released, Duration::from_secs(10)).unwrap();
                assert!(!wait.timed_out(), "timed out waiting to release block {block_number}");
                released = next;
            }
        });
        let tracer = Arc::new(
            DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 2)
                .with_replay_test_probe(probe),
        );

        let first_request = {
            let tracer = tracer.clone();
            tokio::spawn(async move { tracer.debank_block(BlockId::number(1)).await })
        };
        let second_request = {
            let tracer = tracer.clone();
            tokio::spawn(async move { tracer.debank_block(BlockId::number(2)).await })
        };

        let first_entered = tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second_entered = tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(HashSet::from([first_entered, second_entered]), HashSet::from([1, 2]));

        let (lock, wake) = &*released;
        lock.lock().unwrap().extend([1, 2]);
        wake.notify_all();

        let (first_result, second_result) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(first_request, second_request)
        })
        .await
        .unwrap();
        first_result.unwrap().unwrap();
        second_result.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slow_http_client_does_not_retain_replay_permit() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        let genesis = genesis_replay_block(&chain_spec);
        let parent = empty_replay_block(1, genesis.hash());
        let replayed = empty_replay_block(2, parent.hash());

        for block in [&genesis, &parent, &replayed] {
            provider.add_block(block.hash(), block.clone().into_block());
            provider.add_receipts(block.number(), vec![]);
        }
        let cache = cache_replay_blocks(&provider, vec![genesis.clone(), parent, replayed]).await;
        let blocking_pool =
            BlockingTaskPool::new(BlockingTaskPool::builder().num_threads(2).build().unwrap());
        let eth = EthApiBuilder::new(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(blocking_pool)
        .build();

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let tracer = DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1)
            .with_replay_test_probe(ReplayTestProbe::new(move |block_number| {
                entered_tx.send(block_number).unwrap();
            }));
        let server_config =
            ServerConfig::builder().http_only().max_response_body_size(8 * 1024 * 1024).build();
        let server = ServerBuilder::with_config(server_config).build("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let server_handle = server.start(tracer.into_rpc());

        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(1024).unwrap();
        let mut slow_client = socket.connect(server_addr).await.unwrap();
        let genesis_id = serde_json::to_string(&BlockId::number(0)).unwrap();
        let batch_body = format!(
            "[{}]",
            (0..8)
                .map(|id| format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"trace_debankBlock","params":[{genesis_id}]}}"#
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {server_addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{batch_body}",
            batch_body.len()
        );
        slow_client.write_all(request.as_bytes()).await.unwrap();

        let response_headers = tokio::time::timeout(Duration::from_secs(30), async {
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                slow_client.read_exact(&mut byte).await.unwrap();
                headers.push(byte[0]);
            }
            String::from_utf8(headers).unwrap()
        })
        .await
        .unwrap();
        assert!(response_headers.starts_with("HTTP/1.1 200"));
        let content_length = response_headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .expect("JSON-RPC HTTP response must include Content-Length");
        assert!(
            content_length > 1024 * 1024,
            "Genesis batch must exceed socket buffers to exercise slow delivery"
        );

        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(format!("http://{server_addr}"))
            .unwrap();
        let second_request = tokio::spawn(async move {
            client
                .request::<serde_json::Value, _>(
                    "trace_debankBlock",
                    rpc_params![BlockId::number(2)],
                )
                .await
        });
        let entered = tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entered, 2);
        let second_response = tokio::time::timeout(Duration::from_secs(10), second_request)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(second_response.is_object());

        let mut batch_response = vec![0; content_length];
        tokio::time::timeout(Duration::from_secs(30), slow_client.read_exact(&mut batch_response))
            .await
            .expect("slow client should eventually read the complete batch")
            .expect("batch response body should remain available");
        let batch_response: serde_json::Value =
            serde_json::from_slice(&batch_response).expect("batch response must be valid JSON");
        let responses = batch_response.as_array().expect("batch response must be a JSON array");
        assert_eq!(responses.len(), 8);
        assert!(
            responses
                .iter()
                .all(|response| response["result"].is_object() && response.get("error").is_none())
        );

        drop(slow_client);
        server_handle.stop().unwrap();
        server_handle.stopped().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_http_response_returns_complete_error_and_releases_permit() {
        let chain_spec = TAIKO_MAINNET.clone();
        let provider =
            MockEthProvider::<EthPrimitives>::new().with_chain_spec(chain_spec.as_ref().clone());
        let genesis = genesis_replay_block(&chain_spec);
        provider.add_block(genesis.hash(), genesis.clone().into_block());
        provider.add_receipts(genesis.number(), vec![]);
        let cache = cache_replay_blocks(&provider, vec![genesis]).await;
        let eth = EthApiBuilder::new(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            TaikoEvmConfig::new(chain_spec),
        )
        .eth_cache(cache)
        .blocking_task_pool(BlockingTaskPool::new(
            BlockingTaskPool::builder().num_threads(1).build().unwrap(),
        ))
        .build();

        let tracer = DebankTraceExt::<_, InMemoryProofsStorage>::new(eth, None, 1);
        let replay_guard = tracer.replay_guard.clone();
        let server_config =
            ServerConfig::builder().http_only().max_response_body_size(1024).build();
        let server = ServerBuilder::with_config(server_config).build("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let server_handle = server.start(tracer.into_rpc());
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(format!("http://{server_addr}"))
            .unwrap();

        let error = client
            .request::<serde_json::Value, _>("trace_debankBlock", rpc_params![BlockId::number(0)])
            .await
            .expect_err("Genesis response must exceed the configured HTTP limit");
        let ClientError::Call(error) = error else {
            panic!("oversized response must be a complete JSON-RPC error");
        };
        assert_eq!(error.code(), -32_008);
        assert_eq!(error.message(), "Response is too big");

        let permit = tokio::time::timeout(Duration::from_secs(10), replay_guard.acquire_owned())
            .await
            .expect("oversized response must release the replay permit")
            .expect("replay permit should be reusable");
        drop(permit);
        server_handle.stop().unwrap();
        server_handle.stopped().await;
    }

    #[test]
    fn blockfile_create_transaction_uses_created_contract_address() {
        let signer = Address::repeat_byte(0x11);
        let recipient = Address::repeat_byte(0x22);

        assert_eq!(blockfile_transaction_to(TxKind::Create, signer, 7), signer.create(7));
        assert_eq!(blockfile_transaction_to(TxKind::Call(recipient), signer, 7), recipient);
    }

    #[test]
    fn tracing_config_keeps_precompiles_logs_and_sstore_only() {
        let config = debank_tracing_config();
        assert!(!config.exclude_precompile_calls);
        assert!(config.record_logs);
        assert!(config.record_steps);
        let filter = config.record_opcodes_filter.unwrap();
        assert!(filter.is_enabled(reth_revm::bytecode::opcode::OpCode::SSTORE));
        assert!(!filter.is_enabled(reth_revm::bytecode::opcode::OpCode::SLOAD));
    }

    #[test]
    fn debank_transaction_type_rejects_only_eip4844() {
        assert!(!is_unsupported_debank_transaction(&TxLegacy::default()));
        assert!(!is_unsupported_debank_transaction(&TxEip2930::default()));
        assert!(!is_unsupported_debank_transaction(&TxEip1559::default()));
        assert!(is_unsupported_debank_transaction(&TxEip4844::default()));
        assert!(!is_unsupported_debank_transaction(&TxEip7702::default()));
    }

    #[test]
    fn blockfile_fee_caps_match_transaction_type() {
        assert_eq!(blockfile_fee_caps(&TxLegacy { gas_price: 41, ..Default::default() }), (0, 0));
        assert_eq!(blockfile_fee_caps(&TxEip2930 { gas_price: 42, ..Default::default() }), (0, 0));
        assert_eq!(
            blockfile_fee_caps(&TxEip1559 {
                max_fee_per_gas: 43,
                max_priority_fee_per_gas: 4,
                ..Default::default()
            }),
            (43, 4)
        );
        assert_eq!(
            blockfile_fee_caps(&TxEip7702 {
                max_fee_per_gas: 44,
                max_priority_fee_per_gas: 5,
                ..Default::default()
            }),
            (44, 5)
        );
    }

    #[test]
    fn stored_consensus_requires_taiko_protocol_commitments() {
        let make_block = |is_unzen_active: bool| {
            RecoveredBlock::new_unhashed(
                Block {
                    header: Header {
                        transactions_root: EMPTY_TRANSACTIONS,
                        receipts_root: EMPTY_RECEIPTS,
                        withdrawals_root: Some(EMPTY_WITHDRAWALS),
                        parent_beacon_block_root: is_unzen_active
                            .then_some(alloy_primitives::B256::ZERO),
                        blob_gas_used: is_unzen_active.then_some(0),
                        excess_blob_gas: is_unzen_active.then_some(0),
                        requests_hash: is_unzen_active.then_some(EMPTY_REQUESTS_HASH),
                        ..Default::default()
                    },
                    body: AlloyBlockBody {
                        withdrawals: Some(Default::default()),
                        ..Default::default()
                    },
                },
                vec![],
            )
        };

        let pre_unzen = make_block(false);
        assert!(validate_stored_consensus(&pre_unzen, &[], false).is_ok());
        let unzen = make_block(true);
        assert!(validate_stored_consensus(&unzen, &[], true).is_ok());

        let mut non_empty_withdrawals = make_block(true).into_block();
        let withdrawals = Withdrawals::new(vec![Withdrawal {
            index: 1,
            validator_index: 2,
            address: Address::with_last_byte(0x42),
            amount: 3,
        }]);
        non_empty_withdrawals.header.withdrawals_root =
            Some(alloy_consensus::proofs::calculate_withdrawals_root(&withdrawals));
        non_empty_withdrawals.body.withdrawals = Some(withdrawals);
        let non_empty_withdrawals = RecoveredBlock::new_unhashed(non_empty_withdrawals, vec![]);
        assert!(validate_stored_consensus(&non_empty_withdrawals, &[], true).is_ok());

        let mut wrong_non_empty_withdrawals = non_empty_withdrawals.clone().into_block();
        wrong_non_empty_withdrawals.body.withdrawals.as_mut().unwrap()[0].amount = 4;
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(wrong_non_empty_withdrawals, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut missing_withdrawals = unzen.clone().into_block();
        missing_withdrawals.body.withdrawals = None;
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(missing_withdrawals, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut wrong_withdrawals_root = unzen.clone().into_block();
        wrong_withdrawals_root.header.withdrawals_root = None;
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(wrong_withdrawals_root, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut missing_withdrawals_and_root = unzen.clone().into_block();
        missing_withdrawals_and_root.body.withdrawals = None;
        missing_withdrawals_and_root.header.withdrawals_root = None;
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(missing_withdrawals_and_root, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut missing_requests_hash = unzen.into_block();
        missing_requests_hash.header.requests_hash = None;
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(missing_requests_hash, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut wrong_requests_hash = make_block(true).into_block();
        wrong_requests_hash.header.requests_hash = Some(alloy_primitives::B256::repeat_byte(0x11));
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(wrong_requests_hash, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut missing_parent_beacon_root = make_block(true).into_block();
        missing_parent_beacon_root.header.parent_beacon_block_root = None;
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(missing_parent_beacon_root, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut wrong_parent_beacon_root = make_block(true).into_block();
        wrong_parent_beacon_root.header.parent_beacon_block_root =
            Some(alloy_primitives::B256::repeat_byte(0x22));
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(wrong_parent_beacon_root, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut missing_blob_commitment = make_block(true).into_block();
        missing_blob_commitment.header.blob_gas_used = None;
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(missing_blob_commitment, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut unexpected_block_access_list = make_block(true).into_block();
        unexpected_block_access_list.header.block_access_list_hash =
            Some(alloy_primitives::B256::repeat_byte(0x33));
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(unexpected_block_access_list, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut unexpected_slot_number = make_block(true).into_block();
        unexpected_slot_number.header.slot_number = Some(1);
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(unexpected_slot_number, vec![]),
                &[],
                true,
            )
            .is_err()
        );

        let mut pre_unzen_difficulty = make_block(false).into_block();
        pre_unzen_difficulty.header.difficulty = alloy_primitives::U256::from(1);
        assert!(
            validate_stored_consensus(
                &RecoveredBlock::new_unhashed(pre_unzen_difficulty, vec![]),
                &[],
                false,
            )
            .is_err()
        );
    }
}
