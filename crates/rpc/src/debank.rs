//! DeBank block-file wire types and trace formatter.

use crate::geth_error::requires_exact_error;
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B64, B256, Bloom, Bytes, U256, keccak256};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use reth_primitives_traits::{Block, RecoveredBlock};
use reth_revm::{bytecode::opcode::OpCode, interpreter::InstructionResult};
use revm_inspectors::tracing::{
    CallTraceArena,
    types::{CallKind, CallLog, CallTraceNode, TraceMemberOrder},
};
use serde::{Deserialize, Serialize};
use sha1::Digest;

/// Hash adapter used by the complete-state trie root calculation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DebankKeccakHasher;

impl hash_db::Hasher for DebankKeccakHasher {
    type Out = B256;
    type StdHasher = plain_hasher::PlainHasher;

    const LENGTH: usize = 32;

    fn hash(input: &[u8]) -> Self::Out {
        keccak256(input)
    }
}

/// RLP payload consumed by the DeBank state-diff pipeline.
#[derive(Clone, Debug, Default, PartialEq, Eq, RlpDecodable, RlpEncodable)]
pub struct BlockStorageDiff {
    /// Post-block state root.
    pub hash: B256,
    /// Parent state root.
    pub parent_hash: B256,
    /// Final account values keyed by hashed address.
    pub new_accounts: Vec<NewAccount>,
    /// Hashed addresses whose old account and storage must be removed first.
    pub deleted_accounts: Vec<B256>,
    /// Final changed storage values keyed by hashed address and slot.
    pub storage_diffs: Vec<AccountStorageDiff>,
    /// Bytecode required to materialize the final accounts.
    pub new_codes: Vec<NewCode>,
}

/// One bytecode entry in a state diff.
#[derive(Clone, Debug, PartialEq, Eq, RlpDecodable, RlpEncodable)]
pub struct NewCode {
    /// Keccak-256 of `code`.
    pub code_hash: B256,
    /// Original EVM bytecode.
    pub code: Bytes,
}

/// One final account value in a state diff.
#[derive(Clone, Debug, PartialEq, Eq, RlpDecodable, RlpEncodable)]
pub struct NewAccount {
    /// Keccak-256 of the raw address.
    pub address: B256,
    /// Final balance.
    pub balance: U256,
    /// Final nonce.
    pub nonce: u64,
    /// Final code hash.
    pub code_hash: B256,
}

/// Changed storage for one account.
#[derive(Clone, Debug, PartialEq, Eq, RlpDecodable, RlpEncodable)]
pub struct AccountStorageDiff {
    /// Keccak-256 of the raw address.
    pub address: B256,
    /// Changed final slot values.
    pub diffs: Vec<IndexValuePair>,
}

/// One hashed storage key and its final value.
#[derive(Clone, Debug, PartialEq, Eq, RlpDecodable, RlpEncodable)]
pub struct IndexValuePair {
    /// Keccak-256 of the raw storage key.
    pub index: B256,
    /// Final storage value, including zero for deletion.
    pub value: U256,
}

/// Block metadata embedded in `block_file`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct DebankBlock {
    /// Canonical block hash.
    pub id: B256,
    /// Block number.
    pub height: u64,
    /// Canonical parent hash.
    pub parent_id: B256,
    /// EIP-1559 base fee, or zero before activation.
    pub base_fee_per_gas: u64,
    /// Header beneficiary.
    pub miner: Address,
    /// Header gas limit.
    pub gas_limit: u64,
    /// Header gas used.
    pub gas_used: u64,
    /// Header timestamp.
    pub timestamp: u64,
    /// Wall-clock start time in milliseconds.
    pub process_start_timestamp: u128,
}

impl<B: Block> From<&RecoveredBlock<B>> for DebankBlock {
    fn from(block: &RecoveredBlock<B>) -> Self {
        Self {
            id: block.hash(),
            height: block.header().number(),
            parent_id: block.header().parent_hash(),
            base_fee_per_gas: block.header().base_fee_per_gas().unwrap_or_default(),
            miner: block.header().beneficiary(),
            gas_limit: block.header().gas_limit(),
            gas_used: block.header().gas_used(),
            timestamp: block.header().timestamp(),
            process_start_timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        }
    }
}

/// Transaction entry embedded in `block_file`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct DebankTransaction {
    /// Transaction hash.
    pub id: String,
    /// Recovered sender.
    #[serde(rename = "from_addr")]
    pub from: Address,
    /// Recipient, or the created contract address for contract creation.
    #[serde(rename = "to_addr")]
    pub to: Address,
    /// Transaction gas limit.
    pub gas_limit: u64,
    /// Effective gas price.
    pub gas_price: u128,
    /// Receipt gas used, not cumulative gas.
    pub gas_used: u64,
    /// Receipt success status.
    pub status: bool,
    /// EIP-1559 max fee.
    #[serde(rename = "max_fee_per_gas")]
    pub gas_fee_cap: u128,
    /// EIP-1559 priority fee.
    #[serde(rename = "max_priority_fee_per_gas")]
    pub gas_tip_cap: u128,
    /// Calldata or initcode.
    pub input: Bytes,
    /// Transaction nonce.
    pub nonce: u64,
    /// Position in the block.
    #[serde(rename = "idx")]
    pub transaction_index: u64,
    /// Native token value.
    pub value: U256,
}

/// Log entry embedded in `block_file`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DebankEvent {
    /// Deterministic MD5 identifier.
    pub id: String,
    /// Address whose storage context emitted the log.
    pub contract_id: Address,
    /// First log topic, or the empty string for LOG0.
    pub selector: String,
    /// Remaining topics. LOG0 is `null`; LOG1 is `[]`.
    pub topics: Option<Vec<String>>,
    /// Log data.
    pub data: Bytes,
    /// Identifier of the containing trace.
    pub parent_trace_id: String,
    /// Position among projected Call/Log members of the parent.
    pub pos_in_parent_trace: usize,
    /// Canonical block-wide surviving-log index; error events use zero.
    pub idx: usize,
}

/// Call-frame entry embedded in `block_file`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DebankTrace {
    /// Deterministic MD5 identifier.
    pub id: String,
    /// Caller address.
    pub from_addr: Address,
    /// Frame gas limit from the inspector.
    pub gas_limit: u64,
    /// Frame input.
    pub input: Bytes,
    /// Frame target or created address.
    pub to_addr: Address,
    /// Transferred value.
    pub value: U256,
    /// Frame gas used from the inspector.
    pub gas_used: u64,
    /// Return or revert bytes.
    pub output: Bytes,
    /// `call`, `create`, or `suicide`; CREATE2 also uses historical `create`.
    #[serde(rename = "type")]
    pub call_create_type: String,
    /// Lower-case call opcode for call frames.
    pub call_type: String,
    /// Transaction hash.
    pub tx_id: String,
    /// Parent trace identifier, empty for the root.
    pub parent_trace_id: String,
    /// Position among projected Call/Log members of the parent.
    pub pos_in_parent_trace: usize,
    /// Whether this frame executed SSTORE.
    pub self_storage_change: bool,
    /// Whether this frame or a successful child executed SSTORE.
    pub storage_change: bool,
    /// Number of direct call/selfdestruct traces.
    pub subtraces: usize,
    /// Parity-style call-only trace address.
    pub trace_address: Vec<usize>,
    /// Stable pipeline error text, omitted on success.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// Validation object shipped alongside a block file.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BlockValidation {
    /// Last six decimal digits of the SHA-1 integer sum.
    pub validation_hash: i64,
    /// Whether this object represents a fork block.
    pub is_fork: bool,
}

/// Header shape emitted by pipeline v0.0.63.
///
/// This intentionally differs from the standard Ethereum RPC header: EIP-7685 is serialized
/// under the historical `requestsRoot` key, and newer fields outside the pipeline schema are not
/// included.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebankHeader {
    /// Block number.
    #[serde(with = "alloy_serde::quantity")]
    pub number: u64,
    /// Canonical block hash.
    pub hash: B256,
    /// Canonical parent hash.
    pub parent_hash: B256,
    /// Proof-of-work nonce retained by the Ethereum header schema.
    pub nonce: B64,
    /// PREVRANDAO / historical mix digest.
    pub mix_hash: B256,
    /// Empty ommers-list hash on Taiko.
    pub sha3_uncles: B256,
    /// Receipt logs bloom.
    pub logs_bloom: Bloom,
    /// Post-block state root.
    pub state_root: B256,
    /// Header beneficiary.
    pub miner: Address,
    /// Taiko difficulty, including finalized Unzen zk gas.
    pub difficulty: U256,
    /// Header extra data.
    pub extra_data: Bytes,
    /// Block gas limit.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_limit: u64,
    /// Transaction gas used.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,
    /// Block timestamp.
    #[serde(with = "alloy_serde::quantity")]
    pub timestamp: u64,
    /// Transactions trie root.
    pub transactions_root: B256,
    /// Receipts trie root.
    pub receipts_root: B256,
    /// EIP-1559 base fee.
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub base_fee_per_gas: Option<u64>,
    /// EIP-4895 withdrawals root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawals_root: Option<B256>,
    /// EIP-4844 blob gas used.
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub blob_gas_used: Option<u64>,
    /// EIP-4844 excess blob gas.
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub excess_blob_gas: Option<u64>,
    /// EIP-4788 parent beacon block root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_beacon_block_root: Option<B256>,
    /// EIP-7685 requests commitment under the pipeline's historical key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_root: Option<B256>,
}

/// Complete DeBank block-file object.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct BlockFile {
    /// Block metadata.
    pub block: DebankBlock,
    /// Transactions, serialized under the historical `txs` key.
    #[serde(rename = "txs")]
    pub transactions: Vec<DebankTransaction>,
    /// Logs whose full ancestor chain succeeded.
    pub events: Vec<DebankEvent>,
    /// Call frames whose full ancestor chain succeeded.
    pub traces: Vec<DebankTrace>,
    /// Logs reverted by their frame or an ancestor.
    pub error_events: Vec<DebankEvent>,
    /// Call frames reverted by their frame or an ancestor.
    pub error_traces: Vec<DebankTrace>,
    /// Addresses that executed a storage write, compared as a set.
    pub storage_contracts: Vec<Address>,
}

impl BlockFile {
    /// Builds the deterministic companion validation object.
    pub fn validation(&self) -> BlockValidation {
        let mut ids = vec![self.block.id.to_string()];
        ids.extend(self.transactions.iter().map(|tx| tx.id.clone()));
        ids.extend(self.events.iter().map(|event| event.id.clone()));
        ids.extend(self.traces.iter().map(|trace| trace.id.clone()));
        BlockValidation { validation_hash: calc_validation_hash(&ids), is_fork: false }
    }
}

/// Complete `trace_debankBlock` result.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DebankOutput {
    /// Historical pipeline block-file payload.
    pub block_file: BlockFile,
    /// Canonical shipped header.
    pub header: DebankHeader,
    /// RLP-encoded [`BlockStorageDiff`].
    pub state_diff: Bytes,
    /// Validation hash derived from the successful objects.
    pub validation_hash: i64,
}

/// Computes a DeBank object identifier from concatenated string fields.
pub fn debank_id(parts: &[&str]) -> String {
    format!("{:x}", md5::compute(parts.join("").as_bytes()))
}

/// Computes the historical validation hash.
pub fn calc_validation_hash(ids: &[String]) -> i64 {
    let sum = ids.iter().fold(U256::ZERO, |sum, id| {
        let digest = sha1::Sha1::digest(id.as_bytes());
        sum + U256::from_be_slice(&digest)
    });
    let decimal = sum.to_string();
    decimal[decimal.len().saturating_sub(6)..].parse().unwrap_or_default()
}

/// Maps an EVM instruction result and return data to Geth-compatible pipeline text.
///
/// Returns an error when the instruction result does not contain enough information to reproduce
/// Geth's exact error text.
pub fn format_error(result: InstructionResult, output: &[u8]) -> Result<Option<String>, String> {
    if result.is_ok() {
        return Ok(None);
    }
    let message = match result {
        InstructionResult::Revert => decode_revert_reason(output)
            .filter(|reason| !reason.is_empty())
            .map(|reason| format!("execution reverted: {reason}"))
            .unwrap_or_else(|| "execution reverted".into()),
        InstructionResult::OutOfGas |
        InstructionResult::PrecompileOOG |
        InstructionResult::MemoryOOG => "out of gas".into(),
        InstructionResult::ReentrancySentryOOG => {
            "out of gas: not enough gas for reentrancy sentry".into()
        }
        InstructionResult::InvalidOperandOOG => "gas uint64 overflow".into(),
        InstructionResult::CallTooDeep => "max call depth exceeded".into(),
        InstructionResult::OutOfFunds => "insufficient balance for transfer".into(),
        InstructionResult::InvalidFEOpcode => "invalid opcode: INVALID".into(),
        InstructionResult::CallNotAllowedInsideStatic |
        InstructionResult::StateChangeDuringStaticCall => "write protection".into(),
        InstructionResult::InvalidJump => "invalid jump destination".into(),
        InstructionResult::OutOfOffset => "return data out of bounds".into(),
        InstructionResult::CreateCollision => "contract address collision".into(),
        InstructionResult::NonceOverflow => "nonce uint64 overflow".into(),
        InstructionResult::CreateContractSizeLimit => "max code size exceeded".into(),
        InstructionResult::CreateContractStartingWithEF => {
            "invalid code: must not begin with 0xef".into()
        }
        InstructionResult::CreateInitCodeSizeLimit => "max initcode size exceeded".into(),
        unsupported => {
            return Err(format!(
                "cannot reproduce Geth error text from instruction result {unsupported:?}"
            ));
        }
    };
    Ok(Some(message))
}

/// Decodes Solidity's standard `Error(string)` and `Panic(uint256)` revert payloads.
fn decode_revert_reason(output: &[u8]) -> Option<String> {
    const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
    const PANIC_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];
    if output.len() < 4 {
        return None;
    }
    if output[..4] == PANIC_SELECTOR {
        let code = U256::from_be_slice(output.get(4..36)?);
        return Some(match code {
            value if value == U256::from(0x00u64) => "generic panic".into(),
            value if value == U256::from(0x01u64) => "assert(false)".into(),
            value if value == U256::from(0x11u64) => "arithmetic underflow or overflow".into(),
            value if value == U256::from(0x12u64) => "division or modulo by zero".into(),
            value if value == U256::from(0x21u64) => "enum overflow".into(),
            value if value == U256::from(0x22u64) => {
                "invalid encoded storage byte array accessed".into()
            }
            value if value == U256::from(0x31u64) => {
                "out-of-bounds array access; popping on an empty array".into()
            }
            value if value == U256::from(0x32u64) => {
                "out-of-bounds access of an array or bytesN".into()
            }
            value if value == U256::from(0x41u64) => "out of memory".into(),
            value if value == U256::from(0x51u64) => "uninitialized function".into(),
            value => format!("unknown panic code: {value:#x}"),
        });
    }
    if output.len() < 68 || output[..4] != ERROR_STRING_SELECTOR {
        return None;
    }
    let offset = usize::try_from(U256::from_be_slice(&output[4..36])).ok()?;
    let length_offset = 4usize.checked_add(offset)?;
    let data_offset = length_offset.checked_add(32)?;
    if output.len() < data_offset || output.len() < length_offset + 32 {
        return None;
    }
    let length = usize::try_from(U256::from_be_slice(&output[length_offset..data_offset])).ok()?;
    let end = data_offset.checked_add(length)?;
    Some(decode_go_json_string(output.get(data_offset..end)?))
}

/// Decodes arbitrary Go string bytes as they appear after `encoding/json` serialization.
fn decode_go_json_string(input: &[u8]) -> String {
    let mut output = String::with_capacity(input.len());
    for chunk in input.utf8_chunks() {
        output.push_str(chunk.valid());
        for _ in chunk.invalid() {
            output.push(char::REPLACEMENT_CHARACTER);
        }
    }
    output
}

#[derive(Clone, Debug)]
/// A projected block-file member; opcode steps are deliberately absent.
enum Member {
    /// Nested call frame.
    Trace(Box<Node>),
    /// Emitted EVM log.
    Log(DebankEvent),
}

/// Intermediate tree used to apply ancestor success semantics before flattening.
#[derive(Clone, Debug)]
struct Node {
    /// Projected call frame.
    trace: DebankTrace,
    /// Ordered call/log members.
    members: Vec<Member>,
    /// Whether this frame itself succeeded; trace classification ignores ancestors.
    frame_succeeded: bool,
    /// Whether this frame and every ancestor succeeded; log classification uses this.
    events_survive: bool,
}

/// Converts one inspector call node into its wire frame without tree metadata.
fn trace_from_node(node: &CallTraceNode, exact_error: Option<&str>) -> Result<DebankTrace, String> {
    let trace = &node.trace;
    let (kind, call_type) = match trace.kind {
        CallKind::Create | CallKind::Create2 => ("create", String::new()),
        _ => ("call", trace.kind.to_string().to_lowercase()),
    };
    let self_storage_change = trace.steps.iter().any(|step| step.op == OpCode::SSTORE);
    let status =
        trace.status.ok_or_else(|| format!("trace node {} has no final status", node.idx))?;
    // REVM collapses CREATE's code-deposit gas failure into `OutOfGas`, but it preserves the
    // successfully executed initcode output. Geth reports this phase separately.
    if requires_exact_error(status) && exact_error.is_none() {
        return Err(format!(
            "trace node {} ended with {status:?} without exact Geth error context",
            node.idx
        ));
    }
    let error = if let Some(error) = exact_error {
        error.to_owned()
    } else if matches!(trace.kind, CallKind::Create | CallKind::Create2) &&
        status == InstructionResult::OutOfGas &&
        !trace.output.is_empty()
    {
        "contract creation code storage out of gas".into()
    } else {
        format_error(status, &trace.output)?.unwrap_or_default()
    };
    let to_addr = if kind == "create" && !error.is_empty() { Address::ZERO } else { trace.address };
    let output = if status.is_ok() || status == InstructionResult::Revert {
        trace.output.clone()
    } else {
        Bytes::new()
    };
    // Geth consumes the entire frame allowance on exceptional halts, while the inspector keeps
    // the raw gas spent before the failing instruction.
    let gas_used =
        if !trace.success && !status.is_revert() { trace.gas_limit } else { trace.gas_used };
    Ok(DebankTrace {
        from_addr: trace.caller,
        gas_limit: trace.gas_limit,
        input: trace.data.clone(),
        to_addr,
        value: trace.value,
        gas_used,
        output,
        call_create_type: kind.into(),
        call_type,
        self_storage_change,
        storage_change: self_storage_change,
        error,
        ..Default::default()
    })
}

/// Converts one inspector log into its wire event without tree metadata.
fn event_from_log(log: &CallLog) -> DebankEvent {
    let topics = log.raw_log.topics();
    DebankEvent {
        selector: topics.first().map(ToString::to_string).unwrap_or_default(),
        topics: (!topics.is_empty()).then(|| topics[1..].iter().map(ToString::to_string).collect()),
        data: log.raw_log.data.clone(),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
/// Recursively projects the inspector arena while preserving call/log order.
fn build_node(
    tx_id: &str,
    parent_id: &str,
    position: usize,
    node: &CallTraceNode,
    nodes: &[CallTraceNode],
    exact_errors: &[Option<String>],
    ancestors_succeeded: bool,
    trace_address: Vec<usize>,
    log_index: &mut usize,
) -> Result<Node, String> {
    let frame_succeeded = node.trace.success;
    let events_survive = ancestors_succeeded && frame_succeeded;
    let exact_error = exact_errors
        .get(node.idx)
        .ok_or_else(|| format!("missing Geth error sidecar entry for trace node {}", node.idx))?
        .as_deref();
    let mut output = Node {
        trace: trace_from_node(node, exact_error)?,
        members: Vec::new(),
        frame_succeeded,
        events_survive,
    };
    output.trace.tx_id = tx_id.into();
    output.trace.parent_trace_id = parent_id.into();
    output.trace.pos_in_parent_trace = position;
    output.trace.trace_address = trace_address.clone();
    output.trace.id = debank_id(&[tx_id, parent_id, &position.to_string()]);

    for member in &node.ordering {
        match member {
            TraceMemberOrder::Call(index) => {
                let child = &nodes[node.children[*index]];
                let mut address = trace_address.clone();
                address.push(*index);
                let child = build_node(
                    tx_id,
                    &output.trace.id,
                    output.members.len(),
                    child,
                    nodes,
                    exact_errors,
                    events_survive,
                    address,
                    log_index,
                )?;
                if child.trace.storage_change && child.trace.error.is_empty() {
                    output.trace.storage_change = true;
                }
                output.members.push(Member::Trace(Box::new(child)));
            }
            TraceMemberOrder::Log(index) => {
                let mut event = event_from_log(&node.logs[*index]);
                event.contract_id = node.execution_address();
                event.parent_trace_id = output.trace.id.clone();
                event.pos_in_parent_trace = output.members.len();
                event.id =
                    debank_id(&[&event.parent_trace_id, &event.pos_in_parent_trace.to_string()]);
                if events_survive {
                    event.idx = *log_index;
                    *log_index += 1;
                }
                output.members.push(Member::Log(event));
            }
            TraceMemberOrder::Step(_) => {}
        }
    }

    if node.is_selfdestruct() {
        let child_index = node.children.len();
        let mut address = trace_address;
        address.push(child_index);
        let position = output.members.len();
        let mut trace = DebankTrace {
            from_addr: node.trace.selfdestruct_address.unwrap_or_default(),
            to_addr: node.trace.selfdestruct_refund_target.unwrap_or_default(),
            value: node.trace.selfdestruct_transferred_value.unwrap_or_default(),
            call_create_type: "suicide".into(),
            tx_id: tx_id.into(),
            parent_trace_id: output.trace.id.clone(),
            pos_in_parent_trace: position,
            trace_address: address,
            ..Default::default()
        };
        trace.id = debank_id(&[tx_id, &trace.parent_trace_id, &position.to_string()]);
        output.members.push(Member::Trace(Box::new(Node {
            trace,
            members: Vec::new(),
            frame_succeeded: true,
            events_survive,
        })));
    }
    output.trace.subtraces =
        output.members.iter().filter(|member| matches!(member, Member::Trace(_))).count();
    Ok(output)
}

/// Appends one trace to the array selected by its own execution status.
fn append_trace(
    trace: DebankTrace,
    frame_succeeded: bool,
    traces: &mut Vec<DebankTrace>,
    error_traces: &mut Vec<DebankTrace>,
) {
    if frame_succeeded {
        traces.push(trace);
    } else {
        error_traces.push(trace);
    }
}

/// Flattens descendants in the historical Go producer's post-order array layout.
fn flatten_descendants(
    node: Node,
    traces: &mut Vec<DebankTrace>,
    error_traces: &mut Vec<DebankTrace>,
    events: &mut Vec<DebankEvent>,
    error_events: &mut Vec<DebankEvent>,
) {
    let mut direct_traces = Vec::new();
    let mut logs = Vec::new();
    for member in node.members {
        match member {
            Member::Trace(child) => {
                direct_traces.push((child.trace.clone(), child.frame_succeeded));
                flatten_descendants(*child, traces, error_traces, events, error_events);
            }
            Member::Log(event) => logs.push(event),
        }
    }
    for event in logs {
        if node.events_survive {
            events.push(event);
        } else {
            error_events.push(event);
        }
    }
    for (trace, frame_succeeded) in direct_traces {
        append_trace(trace, frame_succeeded, traces, error_traces);
    }
}

/// Rejects incomplete or disconnected trace arenas before recursive formatting.
fn validate_trace_arena(nodes: &[CallTraceNode]) -> Result<(), String> {
    let root = nodes.first().ok_or_else(|| "trace arena is empty".to_string())?;
    if root.idx != 0 || root.parent.is_some() {
        return Err("trace arena root must have index zero and no parent".into());
    }

    let mut visited = vec![false; nodes.len()];
    let mut pending = vec![0usize];
    while let Some(node_index) = pending.pop() {
        if visited[node_index] {
            return Err(format!("trace arena node {node_index} is referenced more than once"));
        }
        visited[node_index] = true;
        let node = &nodes[node_index];
        if node.idx != node_index {
            return Err(format!(
                "trace arena node at position {node_index} declares index {}",
                node.idx
            ));
        }
        let status = node
            .trace
            .status
            .ok_or_else(|| format!("trace node {node_index} has no final status"))?;
        if status.is_ok() != node.trace.success {
            return Err(format!("trace node {node_index} success flag disagrees with final status"));
        }

        let mut ordered_calls = vec![false; node.children.len()];
        let mut ordered_logs = vec![false; node.logs.len()];
        for member in &node.ordering {
            match member {
                TraceMemberOrder::Call(position) => {
                    let seen = ordered_calls.get_mut(*position).ok_or_else(|| {
                        format!(
                            "trace node {node_index} references missing child position {position}"
                        )
                    })?;
                    if std::mem::replace(seen, true) {
                        return Err(format!(
                            "trace node {node_index} repeats child position {position}"
                        ));
                    }
                    let child_index = node.children[*position];
                    let child = nodes.get(child_index).ok_or_else(|| {
                        format!("trace node {node_index} references missing child {child_index}")
                    })?;
                    if child.parent != Some(node_index) {
                        return Err(format!(
                            "trace node {child_index} has parent {:?}, expected {node_index}",
                            child.parent
                        ));
                    }
                    pending.push(child_index);
                }
                TraceMemberOrder::Log(position) => {
                    let seen = ordered_logs.get_mut(*position).ok_or_else(|| {
                        format!(
                            "trace node {node_index} references missing log position {position}"
                        )
                    })?;
                    if std::mem::replace(seen, true) {
                        return Err(format!(
                            "trace node {node_index} repeats log position {position}"
                        ));
                    }
                }
                TraceMemberOrder::Step(_) => {}
            }
        }
        if ordered_calls.iter().any(|seen| !seen) {
            return Err(format!("trace node {node_index} has an unordered child"));
        }
        if ordered_logs.iter().any(|seen| !seen) {
            return Err(format!("trace node {node_index} has an unordered log"));
        }
    }
    if visited.iter().any(|seen| !seen) {
        return Err("trace arena contains a disconnected node".into());
    }
    Ok(())
}

/// Successful traces, failed traces, surviving events, and reverted events for one transaction.
pub type DebankTraceArrays =
    (Vec<DebankTrace>, Vec<DebankTrace>, Vec<DebankEvent>, Vec<DebankEvent>);

/// Converts one transaction's inspector arena to deterministic block-file arrays.
///
/// Invalid arenas or mismatched node-indexed error context are rejected instead of producing
/// partial arrays.
pub fn build_debank_traces(
    tx_hash: B256,
    arena: CallTraceArena,
    exact_errors: &[Option<String>],
    log_index: &mut usize,
) -> Result<DebankTraceArrays, String> {
    let nodes = arena.into_nodes();
    if exact_errors.len() != nodes.len() {
        return Err(format!(
            "Geth error sidecar has {} nodes, trace arena has {}",
            exact_errors.len(),
            nodes.len()
        ));
    }
    validate_trace_arena(&nodes)?;
    let root = build_node(
        &tx_hash.to_string(),
        "",
        0,
        &nodes[0],
        &nodes,
        exact_errors,
        true,
        vec![],
        log_index,
    )?;
    let mut traces = Vec::new();
    let mut error_traces = Vec::new();
    let mut events = Vec::new();
    let mut error_events = Vec::new();
    append_trace(root.trace.clone(), root.frame_succeeded, &mut traces, &mut error_traces);
    flatten_descendants(root, &mut traces, &mut error_traces, &mut events, &mut error_events);
    Ok((traces, error_traces, events, error_events))
}

/// Returns every execution address that observed an `SSTORE` opcode.
///
/// This intentionally derives the set from inspector steps rather than the final
/// bundle: no-op, write-restore, and reverted writes are part of the historical
/// block-file contract even though they do not change the final state. A create
/// frame placed in `error_traces` is excluded because its predicted address was
/// never installed in canonical state.
pub fn observed_storage_contracts(
    traces: &[DebankTrace],
    error_traces: &[DebankTrace],
) -> Vec<Address> {
    use std::collections::BTreeSet;

    let successful = traces.iter().filter(|trace| trace.self_storage_change).map(|trace| {
        if trace.call_type == "delegatecall" { trace.from_addr } else { trace.to_addr }
    });
    let reverted = error_traces
        .iter()
        .filter(|trace| {
            trace.self_storage_change &&
                trace.call_create_type != "create" &&
                trace.call_create_type != "create2"
        })
        .map(
            |trace| {
                if trace.call_type == "delegatecall" { trace.from_addr } else { trace.to_addr }
            },
        );
    successful.chain(reverted).collect::<BTreeSet<_>>().into_iter().collect()
}

/// Builds the shipped RPC header without reading live canonical state again.
pub fn build_rpc_header<B: Block>(block: &RecoveredBlock<B>) -> DebankHeader {
    DebankHeader {
        hash: block.hash(),
        number: block.header().number(),
        parent_hash: block.header().parent_hash(),
        nonce: block.header().nonce().unwrap_or_default(),
        mix_hash: block.header().mix_hash().unwrap_or_default(),
        sha3_uncles: block.header().ommers_hash(),
        logs_bloom: block.header().logs_bloom(),
        state_root: block.header().state_root(),
        miner: block.header().beneficiary(),
        difficulty: block.header().difficulty(),
        extra_data: block.header().extra_data().clone(),
        gas_limit: block.header().gas_limit(),
        gas_used: block.header().gas_used(),
        timestamp: block.header().timestamp(),
        transactions_root: block.header().transactions_root(),
        receipts_root: block.header().receipts_root(),
        base_fee_per_gas: block.header().base_fee_per_gas(),
        withdrawals_root: block.header().withdrawals_root(),
        blob_gas_used: block.header().blob_gas_used(),
        excess_blob_gas: block.header().excess_blob_gas(),
        parent_beacon_block_root: block.header().parent_beacon_block_root(),
        requests_root: block.header().requests_hash(),
    }
}

/// Returns the hashed representation of an address used by state diff RLP.
pub fn hashed_address(address: Address) -> B256 {
    keccak256(address.as_slice())
}

/// Returns the hashed representation of a storage key used by state diff RLP.
pub fn hashed_slot(slot: U256) -> B256 {
    keccak256(slot.to_be_bytes::<32>())
}

/// Converts a replay bundle into the minimal complete state-diff payload.
///
/// Destroyed-and-recreated accounts appear in both `deleted_accounts` and
/// `new_accounts`: consumers must wipe the old account/storage before installing
/// the complete final values. Code is emitted only when the parent account cannot
/// prove that it already referenced the final code hash.
pub fn build_storage_diff(
    bundle: &reth_revm::db::BundleState,
    hash: B256,
    parent_hash: B256,
) -> Result<BlockStorageDiff, String> {
    use alloy_consensus::constants::KECCAK_EMPTY;
    use std::collections::{BTreeMap, BTreeSet};

    let mut accounts = Vec::new();
    let mut deleted = Vec::new();
    let mut storage_diffs = Vec::new();
    let mut required_codes = BTreeSet::new();

    for (address, account) in &bundle.state {
        if account.status.is_not_modified() {
            continue;
        }
        let was_destroyed = account.was_destroyed();
        // CREATE followed by SELFDESTRUCT in the same block has no parent or final account.
        // Emitting a deletion for it is root-neutral noise and violates the expected key set.
        if was_destroyed && account.original_info.is_none() && account.info.is_none() {
            continue;
        }
        let hashed = hashed_address(*address);
        if was_destroyed {
            deleted.push(hashed);
        }

        // A deletion-only account has no final storage. Ignore any intermediate slot values
        // retained by the bundle; `deleted_accounts` wipes the complete parent storage trie.
        let mut slots =
            account
                .info
                .as_ref()
                .into_iter()
                .flat_map(|_| account.storage.iter())
                .filter(|(_, slot)| {
                    if was_destroyed { !slot.present_value.is_zero() } else { slot.is_changed() }
                })
                .map(|(key, slot)| IndexValuePair {
                    index: hashed_slot(*key),
                    value: slot.present_value,
                })
                .collect::<Vec<_>>();
        slots.sort_unstable_by_key(|slot| slot.index);

        let info_changed = account.info != account.original_info;
        if let Some(info) = &account.info {
            if info_changed || was_destroyed || !slots.is_empty() {
                accounts.push(NewAccount {
                    address: hashed,
                    balance: info.balance,
                    nonce: info.nonce,
                    code_hash: info.code_hash,
                });
            }
            let parent_proves_code = !was_destroyed &&
                account
                    .original_info
                    .as_ref()
                    .is_some_and(|parent| parent.code_hash == info.code_hash);
            if info.code_hash != KECCAK_EMPTY && !parent_proves_code {
                required_codes.insert(info.code_hash);
            }
        }
        // A destroyed-and-recreated account needs an explicit storage row even
        // when its final storage is empty. The decoder uses the row together
        // with `deleted_accounts` to wipe the parent's complete storage trie.
        if !slots.is_empty() || (was_destroyed && account.info.is_some()) {
            storage_diffs.push(AccountStorageDiff { address: hashed, diffs: slots });
        }
    }

    let contracts = bundle
        .contracts
        .iter()
        .map(|(hash, code)| (*hash, code.original_bytes()))
        .collect::<BTreeMap<_, _>>();
    let mut new_codes = Vec::with_capacity(required_codes.len());
    for code_hash in required_codes {
        let code = contracts
            .get(&code_hash)
            .cloned()
            .ok_or_else(|| format!("missing bytecode for final code hash {code_hash}"))?;
        if keccak256(&code) != code_hash {
            return Err(format!("bytecode does not hash to {code_hash}"));
        }
        new_codes.push(NewCode { code_hash, code });
    }

    accounts.sort_unstable_by_key(|account| account.address);
    deleted.sort_unstable();
    storage_diffs.sort_unstable_by_key(|storage| storage.address);
    Ok(BlockStorageDiff {
        hash,
        parent_hash,
        new_accounts: accounts,
        deleted_accounts: deleted,
        storage_diffs,
        new_codes,
    })
}

/// Builds the deterministic Genesis state diff from chain allocation.
pub fn build_genesis_storage_diff(
    genesis: &alloy_genesis::Genesis,
    state_root: B256,
) -> BlockStorageDiff {
    use alloy_consensus::constants::KECCAK_EMPTY;
    use reth_trie::EMPTY_ROOT_HASH;
    use std::collections::BTreeMap;

    let mut accounts = Vec::with_capacity(genesis.alloc.len());
    let mut storage_diffs = Vec::new();
    let mut codes = BTreeMap::new();
    for (address, account) in &genesis.alloc {
        let code_hash = account.code.as_ref().map_or(KECCAK_EMPTY, |code| {
            let hash = keccak256(code);
            codes.entry(hash).or_insert_with(|| code.clone());
            hash
        });
        let hashed = hashed_address(*address);
        accounts.push(NewAccount {
            address: hashed,
            balance: account.balance,
            nonce: account.nonce.unwrap_or_default(),
            code_hash,
        });
        let mut diffs = account
            .storage
            .as_ref()
            .into_iter()
            .flat_map(|storage| storage.iter())
            .map(|(key, value)| IndexValuePair {
                index: keccak256(key.as_slice()),
                value: U256::from_be_bytes(value.0),
            })
            .collect::<Vec<_>>();
        diffs.sort_unstable_by_key(|slot| slot.index);
        // The shipped Genesis RLP contains one storage-diff row for every
        // account, including accounts whose slot list is empty.
        storage_diffs.push(AccountStorageDiff { address: hashed, diffs });
    }
    accounts.sort_unstable_by_key(|account| account.address);
    storage_diffs.sort_unstable_by_key(|storage| storage.address);
    BlockStorageDiff {
        hash: state_root,
        parent_hash: EMPTY_ROOT_HASH,
        new_accounts: accounts,
        deleted_accounts: Vec::new(),
        storage_diffs,
        new_codes: codes.into_iter().map(|(code_hash, code)| NewCode { code_hash, code }).collect(),
    }
}

/// Returns Genesis addresses with declared storage, sorted for deterministic JSON.
pub fn genesis_storage_contracts(genesis: &alloy_genesis::Genesis) -> Vec<Address> {
    let mut addresses = genesis
        .alloc
        .iter()
        .filter_map(|(address, account)| {
            account.storage.as_ref().is_some_and(|storage| !storage.is_empty()).then_some(*address)
        })
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses
}

/// Decodes one exact RLP object and converts it into a hashed trie update.
///
/// Duplicate accounts, deletions, storage entries, code entries, orphan code,
/// and trailing RLP bytes are rejected before root computation.
pub fn decode_storage_diff_exact(
    encoded: &[u8],
) -> Result<(BlockStorageDiff, reth_trie::HashedPostState), String> {
    use alloy_rlp::Decodable;
    use reth_primitives_traits::Account;
    use reth_trie::HashedStorage;
    use std::collections::{BTreeMap, BTreeSet};

    let mut input = encoded;
    let diff = BlockStorageDiff::decode(&mut input).map_err(|error| error.to_string())?;
    if !input.is_empty() {
        return Err(format!("state diff has {} trailing RLP bytes", input.len()));
    }

    let mut accounts = BTreeMap::new();
    for account in &diff.new_accounts {
        if accounts
            .insert(
                account.address,
                Account {
                    balance: account.balance,
                    nonce: account.nonce,
                    bytecode_hash: Some(account.code_hash),
                },
            )
            .is_some()
        {
            return Err(format!("duplicate account {}", account.address));
        }
    }
    let mut deleted = BTreeSet::new();
    for address in &diff.deleted_accounts {
        if !deleted.insert(*address) {
            return Err(format!("duplicate deleted account {address}"));
        }
    }

    let mut storages = BTreeMap::new();
    for storage in &diff.storage_diffs {
        if !accounts.contains_key(&storage.address) {
            return Err(format!(
                "storage account {} is missing its final account value",
                storage.address
            ));
        }
        let mut slots = BTreeMap::new();
        for slot in &storage.diffs {
            if slots.insert(slot.index, slot.value).is_some() {
                return Err(format!("duplicate storage slot {}/{}", storage.address, slot.index));
            }
        }
        if storages.insert(storage.address, slots).is_some() {
            return Err(format!("duplicate storage account {}", storage.address));
        }
    }

    let referenced_codes =
        accounts.values().filter_map(|account| account.bytecode_hash).collect::<BTreeSet<_>>();
    let mut codes = BTreeSet::new();
    for code in &diff.new_codes {
        if keccak256(&code.code) != code.code_hash {
            return Err(format!("bytecode does not hash to {}", code.code_hash));
        }
        if !codes.insert(code.code_hash) {
            return Err(format!("duplicate code {}", code.code_hash));
        }
        if !referenced_codes.contains(&code.code_hash) {
            return Err(format!("orphan code {}", code.code_hash));
        }
    }

    let hashed_accounts =
        accounts.into_iter().map(|(address, account)| (address, Some(account))).chain(
            deleted
                .iter()
                .filter(|address| {
                    !diff.new_accounts.iter().any(|account| account.address == **address)
                })
                .map(|address| (*address, None)),
        );
    let mut hashed_storages = deleted
        .iter()
        .map(|address| (*address, HashedStorage::new(true)))
        .collect::<BTreeMap<_, _>>();
    for (address, slots) in storages {
        let storage = hashed_storages.entry(address).or_insert_with(|| HashedStorage::new(false));
        storage.storage.extend(slots);
    }
    let state = reth_trie::HashedPostState::default()
        .with_accounts(hashed_accounts)
        .with_storages(hashed_storages);
    Ok((diff, state))
}

/// Decodes a wire payload and proves that no semantic field changed during encoding.
///
/// The expected value is derived from the replay bundle. Comparing it after strict decoding
/// catches root-metadata mutations and root-invisible changes such as omitted code, an added
/// unchanged account, or an omitted zero-valued slot.
pub fn decode_and_validate_storage_diff_exact(
    encoded: &[u8],
    expected: &BlockStorageDiff,
) -> Result<reth_trie::HashedPostState, String> {
    let (decoded, state) = decode_storage_diff_exact(encoded)?;
    if decoded != *expected {
        return Err("decoded state diff differs from the replay-derived expected set".into());
    }
    Ok(state)
}

/// Computes a state root when the diff describes the entire state, as Genesis does.
pub fn complete_state_root(diff: &BlockStorageDiff) -> Result<B256, String> {
    use alloy_rlp::encode_fixed_size;
    use reth_primitives_traits::Account;
    use std::collections::BTreeMap;

    if !diff.deleted_accounts.is_empty() {
        return Err("complete state cannot contain deleted accounts".into());
    }
    let storage = diff
        .storage_diffs
        .iter()
        .map(|account| (account.address, &account.diffs))
        .collect::<BTreeMap<_, _>>();
    if storage.len() != diff.new_accounts.len() {
        return Err("complete state must contain exactly one storage row per account".into());
    }

    let encoded_accounts = diff.new_accounts.iter().map(|account| {
        let slots = storage
            .get(&account.address)
            .ok_or_else(|| format!("missing complete storage row for {}", account.address))?;
        let encoded_slots = slots.iter().map(|slot| (slot.index, encode_fixed_size(&slot.value)));
        let storage_root = triehash::trie_root::<DebankKeccakHasher, _, _, _>(encoded_slots);
        let address = account.address;
        let trie_account = Account {
            balance: account.balance,
            nonce: account.nonce,
            bytecode_hash: Some(account.code_hash),
        }
        .into_trie_account(storage_root);
        Ok((address, alloy_rlp::encode(trie_account)))
    });
    let encoded_accounts = encoded_accounts.collect::<Result<Vec<_>, String>>()?;
    Ok(triehash::trie_root::<DebankKeccakHasher, _, _, _>(encoded_accounts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_MAINNET, hardfork::TaikoHardforks};
    use alloy_consensus::{Block as AlloyBlock, BlockBody as AlloyBlockBody, Header};
    use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
    use reth::chainspec::EthChainSpec;
    use revm_inspectors::tracing::types::{CallTrace, CallTraceNode, CallTraceStep};

    fn node(
        idx: usize,
        parent: Option<usize>,
        children: Vec<usize>,
        success: bool,
    ) -> CallTraceNode {
        CallTraceNode {
            idx,
            parent,
            children,
            trace: CallTrace {
                success,
                status: Some(if success {
                    InstructionResult::Stop
                } else {
                    InstructionResult::Revert
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn build_test_debank_traces(
        tx_hash: B256,
        arena: CallTraceArena,
        log_index: &mut usize,
    ) -> Result<DebankTraceArrays, String> {
        let exact_errors = vec![None; arena.nodes().len()];
        build_debank_traces(tx_hash, arena, &exact_errors, log_index)
    }

    #[test]
    fn validation_hash_matches_pipeline_vector() {
        let ids = (1..=5).map(|value| value.to_string()).collect::<Vec<_>>();
        assert_eq!(calc_validation_hash(&ids), 391_764);
    }

    #[test]
    fn stable_mainnet_fork_boundaries_are_pinned() {
        assert!(!TAIKO_MAINNET.is_ontake_active_at_block(538_303));
        assert!(TAIKO_MAINNET.is_ontake_active_at_block(538_304));
        assert!(!TAIKO_MAINNET.is_pacaya_active_at_block(1_165_999));
        assert!(TAIKO_MAINNET.is_pacaya_active_at_block(1_166_000));
        assert!(!TAIKO_MAINNET.is_shasta_active(1_775_135_699));
        assert!(TAIKO_MAINNET.is_shasta_active(1_775_135_700));
        assert!(TAIKO_MAINNET.is_shasta_active(1_775_135_701));
        assert!(!TAIKO_MAINNET.is_unzen_active(1_786_021_199));
        assert!(TAIKO_MAINNET.is_unzen_active(1_786_021_200));
        assert!(TAIKO_MAINNET.is_unzen_active(1_786_021_201));
    }

    #[test]
    fn pre_basefee_block_serializes_zero() {
        let value = serde_json::to_value(DebankBlock::default()).unwrap();
        assert_eq!(value["base_fee_per_gas"], serde_json::json!(0));
    }

    #[test]
    fn pipeline_header_uses_requests_root_and_exact_schema() {
        let block = RecoveredBlock::new_unhashed(
            AlloyBlock::<reth_ethereum_primitives::TransactionSigned> {
                header: Header {
                    number: 42,
                    requests_hash: Some(EMPTY_REQUESTS_HASH),
                    parent_beacon_block_root: Some(B256::ZERO),
                    blob_gas_used: Some(0),
                    excess_blob_gas: Some(0),
                    ..Default::default()
                },
                body: AlloyBlockBody::default(),
            },
            vec![],
        );
        let value = serde_json::to_value(build_rpc_header(&block)).unwrap();

        assert_eq!(value["number"], "0x2a");
        assert_eq!(value["blobGasUsed"], "0x0");
        assert_eq!(value["excessBlobGas"], "0x0");
        assert_eq!(value["requestsRoot"], EMPTY_REQUESTS_HASH.to_string());
        assert!(value.get("requestsHash").is_none());
        assert!(value.get("blockAccessListHash").is_none());
        assert!(value.get("slotNumber").is_none());
        assert!(value.get("totalDifficulty").is_none());
        assert!(value.get("size").is_none());
    }

    #[test]
    fn errors_match_geth_pipeline_strings() {
        let mut revert = vec![0x08, 0xc3, 0x79, 0xa0];
        revert.extend([0u8; 31]);
        revert.push(0x20);
        revert.extend([0u8; 31]);
        revert.push(0x05);
        revert.extend(b"X+Y>0");
        revert.extend([0u8; 27]);

        assert_eq!(
            format_error(InstructionResult::Revert, &revert).unwrap().as_deref(),
            Some("execution reverted: X+Y>0")
        );
        revert[67] = 4;
        revert[68..72].copy_from_slice(&[0xc5, b'h', b's', 0xba]);
        assert_eq!(
            format_error(InstructionResult::Revert, &revert).unwrap().as_deref(),
            Some("execution reverted: �hs�")
        );
        revert[67] = 3;
        revert[68..71].copy_from_slice(&[0xf1, 0x80, b'b']);
        assert_eq!(
            format_error(InstructionResult::Revert, &revert).unwrap().as_deref(),
            Some("execution reverted: ��b")
        );
        revert[67] = 0;
        assert_eq!(
            format_error(InstructionResult::Revert, &revert).unwrap().as_deref(),
            Some("execution reverted")
        );
        assert_eq!(
            format_error(InstructionResult::Revert, &[]).unwrap().as_deref(),
            Some("execution reverted")
        );
        assert_eq!(
            format_error(InstructionResult::OutOfGas, &[]).unwrap().as_deref(),
            Some("out of gas")
        );
        assert_eq!(
            format_error(InstructionResult::InvalidFEOpcode, &[]).unwrap().as_deref(),
            Some("invalid opcode: INVALID")
        );
        for (result, expected) in [
            (InstructionResult::CallTooDeep, "max call depth exceeded"),
            (InstructionResult::OutOfFunds, "insufficient balance for transfer"),
            (InstructionResult::InvalidOperandOOG, "gas uint64 overflow"),
            (
                InstructionResult::ReentrancySentryOOG,
                "out of gas: not enough gas for reentrancy sentry",
            ),
            (InstructionResult::StateChangeDuringStaticCall, "write protection"),
            (InstructionResult::OutOfOffset, "return data out of bounds"),
            (InstructionResult::CreateCollision, "contract address collision"),
            (InstructionResult::NonceOverflow, "nonce uint64 overflow"),
            (InstructionResult::CreateContractSizeLimit, "max code size exceeded"),
            (
                InstructionResult::CreateContractStartingWithEF,
                "invalid code: must not begin with 0xef",
            ),
            (InstructionResult::CreateInitCodeSizeLimit, "max initcode size exceeded"),
        ] {
            assert_eq!(format_error(result, &[]).unwrap().as_deref(), Some(expected));
        }
        assert!(format_error(InstructionResult::OpcodeNotFound, &[]).is_err());
        assert!(format_error(InstructionResult::PrecompileError, &[]).is_err());
        assert!(format_error(InstructionResult::StackUnderflow, &[]).is_err());
    }

    #[test]
    fn panic_reasons_match_geth_pipeline_strings() {
        let mut known = vec![0x4e, 0x48, 0x7b, 0x71];
        known.extend(U256::from(0x11u64).to_be_bytes::<32>());
        assert_eq!(
            format_error(InstructionResult::Revert, &known).unwrap().as_deref(),
            Some("execution reverted: arithmetic underflow or overflow")
        );

        let mut unknown = vec![0x4e, 0x48, 0x7b, 0x71];
        unknown.extend(U256::from(0xdeadu64).to_be_bytes::<32>());
        assert_eq!(
            format_error(InstructionResult::Revert, &unknown).unwrap().as_deref(),
            Some("execution reverted: unknown panic code: 0xdead")
        );

        assert_eq!(
            format_error(InstructionResult::Revert, &[0x4e, 0x48, 0x7b, 0x71]).unwrap().as_deref(),
            Some("execution reverted")
        );
    }

    #[test]
    fn formatter_normalizes_exceptional_halts_and_clears_output() {
        let raw_output = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);

        let mut successful = node(0, None, vec![], true);
        successful.trace.gas_limit = 10_000;
        successful.trace.gas_used = 4_000;
        successful.trace.output = raw_output.clone();
        let formatted = trace_from_node(&successful, None).unwrap();
        assert_eq!(formatted.gas_used, 4_000);
        assert_eq!(formatted.output, raw_output);

        let mut reverted = node(0, None, vec![], false);
        reverted.trace.gas_limit = 10_000;
        reverted.trace.gas_used = 4_000;
        reverted.trace.output = raw_output.clone();
        let formatted = trace_from_node(&reverted, None).unwrap();
        assert_eq!(formatted.gas_used, 4_000);
        assert_eq!(formatted.output, raw_output);

        let mut call_too_deep = node(0, None, vec![], false);
        call_too_deep.trace.status = Some(InstructionResult::CallTooDeep);
        call_too_deep.trace.gas_limit = 10_000;
        call_too_deep.trace.gas_used = 4_000;
        assert_eq!(trace_from_node(&call_too_deep, None).unwrap().gas_used, 4_000);

        let mut halted = node(0, None, vec![], false);
        halted.trace.status = Some(InstructionResult::OutOfGas);
        halted.trace.gas_limit = 10_000;
        halted.trace.gas_used = 4_000;
        halted.trace.output = raw_output.clone();
        let formatted = trace_from_node(&halted, None).unwrap();
        assert_eq!(formatted.error, "out of gas");
        assert_eq!(formatted.gas_used, 10_000);
        assert!(formatted.output.is_empty());

        let mut code_deposit_oog = node(0, None, vec![], false);
        code_deposit_oog.trace.kind = CallKind::Create;
        code_deposit_oog.trace.status = Some(InstructionResult::OutOfGas);
        code_deposit_oog.trace.gas_limit = 10_000;
        code_deposit_oog.trace.gas_used = 4_000;
        code_deposit_oog.trace.output = raw_output.clone();
        let formatted = trace_from_node(&code_deposit_oog, None).unwrap();
        assert_eq!(formatted.error, "contract creation code storage out of gas");
        assert_eq!(formatted.gas_used, 10_000);
        assert_eq!(formatted.to_addr, Address::ZERO);
        assert!(formatted.output.is_empty());

        let mut reverted_create = node(0, None, vec![], false);
        reverted_create.trace.kind = CallKind::Create2;
        reverted_create.trace.address = Address::repeat_byte(0x42);
        reverted_create.trace.output = raw_output.clone();
        let formatted = trace_from_node(&reverted_create, None).unwrap();
        assert_eq!(formatted.to_addr, Address::ZERO);
        assert_eq!(formatted.output, raw_output);
    }

    #[test]
    fn formatter_rejects_empty_and_invalid_arenas() {
        let mut empty = CallTraceArena::default();
        empty.nodes_mut().clear();
        assert!(
            build_test_debank_traces(B256::ZERO, empty, &mut 0)
                .unwrap_err()
                .contains("arena is empty")
        );

        let incomplete = CallTraceArena::default();
        assert!(
            build_test_debank_traces(B256::ZERO, incomplete, &mut 0)
                .unwrap_err()
                .contains("no final status")
        );

        let mut root = node(0, None, vec![1], true);
        root.ordering = vec![TraceMemberOrder::Call(1)];
        let mut invalid = CallTraceArena::default();
        *invalid.nodes_mut() = vec![root, node(1, Some(0), vec![], true)];
        assert!(
            build_test_debank_traces(B256::ZERO, invalid, &mut 0)
                .unwrap_err()
                .contains("missing child position")
        );
    }

    #[test]
    fn formatter_requires_and_uses_node_indexed_exact_errors() {
        let mut low_gas_underflow = node(0, None, vec![], false);
        low_gas_underflow.trace.status = Some(InstructionResult::OutOfGas);
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![low_gas_underflow];
        let exact_errors = vec![Some("stack underflow (0 <=> 2)".into())];
        let (_, error_traces, _, _) =
            build_debank_traces(B256::ZERO, arena, &exact_errors, &mut 0).unwrap();
        assert_eq!(error_traces[0].error, "stack underflow (0 <=> 2)");

        let mut missing_context = node(0, None, vec![], false);
        missing_context.trace.status = Some(InstructionResult::OpcodeNotFound);
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![missing_context];
        assert!(
            build_debank_traces(B256::ZERO, arena.clone(), &[None], &mut 0)
                .unwrap_err()
                .contains("without exact Geth error context")
        );
        assert!(
            build_debank_traces(B256::ZERO, arena, &[], &mut 0)
                .unwrap_err()
                .contains("sidecar has 0 nodes")
        );
    }

    #[test]
    fn ids_match_pipeline_vectors() {
        assert_eq!(debank_id(&["abcd", "2"]), "6e24a85785fd5e2688f1a23aee9d88f3");
    }

    #[test]
    fn formatter_omits_steps_from_positions_and_preserves_call_log_order() {
        let mut root = node(0, None, vec![1, 2], true);
        root.trace.gas_limit = 123_456;
        root.logs = vec![CallLog::default(), CallLog::default()];
        root.ordering = vec![
            TraceMemberOrder::Log(0),
            TraceMemberOrder::Call(0),
            TraceMemberOrder::Step(0),
            TraceMemberOrder::Log(1),
            TraceMemberOrder::Call(1),
        ];
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() =
            vec![root, node(1, Some(0), vec![], true), node(2, Some(0), vec![], true)];

        let (traces, error_traces, events, error_events) =
            build_test_debank_traces(B256::repeat_byte(0x11), arena, &mut 0).unwrap();
        assert!(error_traces.is_empty());
        assert!(error_events.is_empty());
        assert_eq!(traces[0].gas_limit, 123_456);
        assert_eq!(traces[0].subtraces, 2);
        assert_eq!(traces[1].pos_in_parent_trace, 1);
        assert_eq!(traces[1].trace_address, vec![0]);
        assert_eq!(traces[2].pos_in_parent_trace, 3);
        assert_eq!(traces[2].trace_address, vec![1]);
        assert_eq!(events[0].pos_in_parent_trace, 0);
        assert_eq!(events[0].idx, 0);
        assert_eq!(events[1].pos_in_parent_trace, 2);
        assert_eq!(events[1].idx, 1);
    }

    #[test]
    fn formatter_moves_reverted_and_ancestor_reverted_logs_to_error_events() {
        let mut root = node(0, None, vec![1], false);
        root.logs = vec![CallLog::default()];
        root.ordering = vec![TraceMemberOrder::Call(0), TraceMemberOrder::Log(0)];
        let mut child = node(1, Some(0), vec![], true);
        child.logs = vec![CallLog::default()];
        child.ordering = vec![TraceMemberOrder::Log(0)];
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![root, child];

        let (traces, error_traces, events, error_events) =
            build_test_debank_traces(B256::repeat_byte(0x22), arena, &mut 0).unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].parent_trace_id, error_traces[0].id);
        assert!(events.is_empty());
        assert_eq!(error_traces.len(), 1);
        assert_eq!(error_events.len(), 2);
        assert!(error_events.iter().all(|event| event.idx == 0));
    }

    #[test]
    fn formatter_keeps_surviving_log_indices_contiguous_across_revert() {
        let mut root = node(0, None, vec![1], true);
        root.logs = vec![CallLog::default(), CallLog::default()];
        root.ordering =
            vec![TraceMemberOrder::Log(0), TraceMemberOrder::Call(0), TraceMemberOrder::Log(1)];
        let mut reverted = node(1, Some(0), vec![], false);
        reverted.logs = vec![CallLog::default()];
        reverted.ordering = vec![TraceMemberOrder::Log(0)];
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![root, reverted];

        let (traces, error_traces, events, error_events) =
            build_test_debank_traces(B256::repeat_byte(0x22), arena, &mut 0).unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(error_traces.len(), 1);
        assert_eq!(events.iter().map(|event| event.idx).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(error_events.len(), 1);
        assert_eq!(error_events[0].idx, 0);
    }

    #[test]
    fn formatter_matches_pipeline_post_order_arrays() {
        let mut root = node(0, None, vec![1], true);
        root.logs = vec![CallLog::default()];
        root.ordering = vec![TraceMemberOrder::Log(0), TraceMemberOrder::Call(0)];
        let mut child = node(1, Some(0), vec![2], true);
        child.logs = vec![CallLog::default()];
        child.ordering = vec![TraceMemberOrder::Log(0), TraceMemberOrder::Call(0)];
        let grandchild = node(2, Some(1), vec![], true);
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![root, child, grandchild];

        let (traces, error_traces, events, error_events) =
            build_test_debank_traces(B256::repeat_byte(0x23), arena, &mut 0).unwrap();
        assert!(error_traces.is_empty());
        assert!(error_events.is_empty());
        assert_eq!(traces.len(), 3);
        assert_eq!(traces[1].parent_trace_id, traces[2].id);
        assert_eq!(traces[2].parent_trace_id, traces[0].id);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].parent_trace_id, traces[2].id);
        assert_eq!(events[0].idx, 1);
        assert_eq!(events[1].parent_trace_id, traces[0].id);
        assert_eq!(events[1].idx, 0);
    }

    #[test]
    fn formatter_uses_create_wire_type_and_clears_failed_target() {
        let mut failed = node(0, None, vec![], false);
        failed.trace.kind = CallKind::Create2;
        failed.trace.address = Address::repeat_byte(0x24);
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![failed];
        let (traces, error_traces, _, _) =
            build_test_debank_traces(B256::repeat_byte(0x24), arena, &mut 0).unwrap();
        assert!(traces.is_empty());
        assert_eq!(error_traces[0].call_create_type, "create");
        assert_eq!(error_traces[0].to_addr, Address::ZERO);

        let mut successful = node(0, None, vec![], true);
        successful.trace.kind = CallKind::Create2;
        successful.trace.address = Address::repeat_byte(0x25);
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![successful];
        let (traces, error_traces, _, _) =
            build_test_debank_traces(B256::repeat_byte(0x25), arena, &mut 0).unwrap();
        assert!(error_traces.is_empty());
        assert_eq!(traces[0].call_create_type, "create");
        assert_eq!(traces[0].to_addr, Address::repeat_byte(0x25));
    }

    #[test]
    fn formatter_places_selfdestruct_after_existing_children() {
        let mut root = node(0, None, vec![1, 2], true);
        root.ordering = vec![TraceMemberOrder::Call(0), TraceMemberOrder::Call(1)];
        root.trace.status = Some(InstructionResult::SelfDestruct);
        root.trace.selfdestruct_refund_target = Some(Address::repeat_byte(0xaa));
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() =
            vec![root, node(1, Some(0), vec![], true), node(2, Some(0), vec![], true)];

        let (traces, _, _, _) =
            build_test_debank_traces(B256::repeat_byte(0x33), arena, &mut 0).unwrap();
        assert_eq!(traces[0].subtraces, 3);
        let suicide = traces.last().unwrap();
        assert_eq!(suicide.call_create_type, "suicide");
        assert_eq!(suicide.pos_in_parent_trace, 2);
        assert_eq!(suicide.trace_address, vec![2]);
    }

    #[test]
    fn log_topic_shapes_are_distinct() {
        let log0 = DebankEvent::default();
        let log1 = DebankEvent {
            selector: B256::ZERO.to_string(),
            topics: Some(vec![]),
            ..Default::default()
        };
        assert!(serde_json::to_value(log0).unwrap()["topics"].is_null());
        assert_eq!(serde_json::to_value(log1).unwrap()["topics"], serde_json::json!([]));
    }

    #[test]
    fn taiko_genesis_diff_has_fixed_complete_sets() {
        let diff = build_genesis_storage_diff(TAIKO_MAINNET.genesis(), B256::repeat_byte(0x11));
        assert_eq!(diff.new_accounts.len(), 22);
        assert_eq!(diff.storage_diffs.len(), 22);
        assert_eq!(diff.storage_diffs.iter().map(|account| account.diffs.len()).sum::<usize>(), 61);
        assert_eq!(diff.new_codes.len(), 13);
        let encoded = alloy_rlp::encode(&diff);
        let (decoded, _) = decode_storage_diff_exact(&encoded).unwrap();
        assert_eq!(decoded.parent_hash, reth_trie::EMPTY_ROOT_HASH);
        assert_eq!(
            complete_state_root(&decoded).unwrap(),
            TAIKO_MAINNET.genesis_header().state_root
        );
        let storage_contracts = genesis_storage_contracts(TAIKO_MAINNET.genesis());
        assert_eq!(storage_contracts.len(), 16);
        assert!(storage_contracts.iter().all(|address| {
            TAIKO_MAINNET.genesis().alloc[address]
                .storage
                .as_ref()
                .is_some_and(|storage| !storage.is_empty())
        }));
    }

    #[test]
    fn observed_storage_contracts_preserve_reverted_and_call_context_writes() {
        let callcode_target = Address::repeat_byte(0x11);
        let delegate_context = Address::repeat_byte(0x22);
        let reverted_target = Address::repeat_byte(0x33);
        let failed_create_target = Address::repeat_byte(0x44);
        let successful = vec![
            DebankTrace {
                to_addr: callcode_target,
                call_type: "callcode".into(),
                self_storage_change: true,
                ..Default::default()
            },
            DebankTrace {
                from_addr: delegate_context,
                to_addr: Address::repeat_byte(0xaa),
                call_type: "delegatecall".into(),
                self_storage_change: true,
                ..Default::default()
            },
        ];
        let reverted = vec![
            DebankTrace {
                to_addr: reverted_target,
                call_type: "call".into(),
                self_storage_change: true,
                error: "Reverted".into(),
                ..Default::default()
            },
            DebankTrace {
                to_addr: failed_create_target,
                call_create_type: "create".into(),
                self_storage_change: true,
                error: "Reverted".into(),
                ..Default::default()
            },
        ];

        assert_eq!(
            observed_storage_contracts(&successful, &reverted),
            vec![callcode_target, delegate_context, reverted_target]
        );
    }

    #[test]
    fn sstore_step_sets_flags_even_when_frame_reverts() {
        let target = Address::repeat_byte(0x45);
        let mut root = node(0, None, vec![], false);
        root.trace.address = target;
        root.trace.steps = vec![CallTraceStep {
            pc: 0,
            op: OpCode::SSTORE,
            stack: None,
            push_stack: None,
            memory: None,
            returndata: Bytes::new(),
            gas_remaining: 0,
            gas_refund_counter: 0,
            gas_used: 0,
            gas_cost: 0,
            storage_change: None,
            status: None,
            immediate_bytes: None,
            decoded: None,
        }];
        let mut arena = CallTraceArena::default();
        *arena.nodes_mut() = vec![root];

        let (traces, error_traces, _, _) =
            build_test_debank_traces(B256::repeat_byte(0x45), arena, &mut 0).unwrap();
        assert!(traces.is_empty());
        assert!(error_traces[0].self_storage_change);
        assert!(error_traces[0].storage_change);
        assert_eq!(observed_storage_contracts(&traces, &error_traces), vec![target]);
    }

    #[test]
    fn exact_decoder_rejects_trailing_rlp_and_duplicates() {
        let mut diff = build_genesis_storage_diff(TAIKO_MAINNET.genesis(), B256::repeat_byte(0x22));
        let mut encoded = alloy_rlp::encode(diff.clone());
        encoded.push(0xc0);
        assert!(decode_storage_diff_exact(&encoded).unwrap_err().contains("trailing RLP"));

        diff.new_accounts.push(diff.new_accounts[0].clone());
        assert!(
            decode_storage_diff_exact(&alloy_rlp::encode(diff))
                .unwrap_err()
                .contains("duplicate account")
        );
    }

    #[test]
    fn exact_decoder_rejects_duplicate_deleted_account() {
        let mut diff = build_genesis_storage_diff(TAIKO_MAINNET.genesis(), B256::repeat_byte(0x22));
        let address = B256::repeat_byte(0x99);
        diff.deleted_accounts.extend([address, address]);

        assert!(
            decode_storage_diff_exact(&alloy_rlp::encode(diff))
                .unwrap_err()
                .contains("duplicate deleted account")
        );
    }

    #[test]
    fn exact_decoder_rejects_duplicate_storage_account_and_slot() {
        let diff = build_genesis_storage_diff(TAIKO_MAINNET.genesis(), B256::repeat_byte(0x22));

        let mut duplicate_account = diff.clone();
        let storage = duplicate_account.storage_diffs[0].clone();
        duplicate_account.storage_diffs.push(storage);
        assert!(
            decode_storage_diff_exact(&alloy_rlp::encode(duplicate_account))
                .unwrap_err()
                .contains("duplicate storage account")
        );

        let mut duplicate_slot = diff;
        let storage = duplicate_slot
            .storage_diffs
            .iter_mut()
            .find(|storage| !storage.diffs.is_empty())
            .expect("Taiko Genesis has non-empty storage");
        let slot = storage.diffs[0].clone();
        storage.diffs.push(slot);
        assert!(
            decode_storage_diff_exact(&alloy_rlp::encode(duplicate_slot))
                .unwrap_err()
                .contains("duplicate storage slot")
        );
    }

    #[test]
    fn exact_decoder_rejects_duplicate_code() {
        let mut diff = build_genesis_storage_diff(TAIKO_MAINNET.genesis(), B256::repeat_byte(0x22));
        let code = diff.new_codes[0].clone();
        diff.new_codes.push(code);

        assert!(
            decode_storage_diff_exact(&alloy_rlp::encode(diff))
                .unwrap_err()
                .contains("duplicate code")
        );
    }

    #[test]
    fn exact_decoder_rejects_orphan_code() {
        let mut diff = build_genesis_storage_diff(TAIKO_MAINNET.genesis(), B256::repeat_byte(0x22));
        let code = Bytes::from_static(b"orphan bytecode");
        let code_hash = keccak256(&code);
        assert!(diff.new_accounts.iter().all(|account| account.code_hash != code_hash));
        diff.new_codes.push(NewCode { code_hash, code });

        assert!(
            decode_storage_diff_exact(&alloy_rlp::encode(diff))
                .unwrap_err()
                .contains("orphan code")
        );
    }

    #[test]
    fn expected_set_validation_rejects_root_invisible_mutations() {
        let expected = build_genesis_storage_diff(TAIKO_MAINNET.genesis(), B256::repeat_byte(0x22));

        let mut mutation = expected.clone();
        mutation.hash = B256::repeat_byte(0x33);
        assert!(
            decode_and_validate_storage_diff_exact(&alloy_rlp::encode(mutation), &expected)
                .unwrap_err()
                .contains("expected set")
        );

        let mut mutation = expected.clone();
        mutation.new_codes.pop();
        assert!(
            decode_and_validate_storage_diff_exact(&alloy_rlp::encode(mutation), &expected)
                .unwrap_err()
                .contains("expected set")
        );

        let mut mutation = expected.clone();
        mutation.new_accounts.push(NewAccount {
            address: B256::repeat_byte(0x99),
            balance: U256::ZERO,
            nonce: 0,
            code_hash: alloy_consensus::constants::KECCAK_EMPTY,
        });
        assert!(
            decode_and_validate_storage_diff_exact(&alloy_rlp::encode(mutation), &expected)
                .unwrap_err()
                .contains("expected set")
        );
    }

    #[test]
    fn exact_decoder_wipes_storage_for_destroy_and_recreate() {
        use alloy_consensus::constants::KECCAK_EMPTY;

        let address = B256::repeat_byte(0x55);
        let diff = BlockStorageDiff {
            hash: B256::repeat_byte(0x66),
            parent_hash: B256::repeat_byte(0x77),
            new_accounts: vec![NewAccount {
                address,
                balance: U256::from(1),
                nonce: 1,
                code_hash: KECCAK_EMPTY,
            }],
            deleted_accounts: vec![address],
            storage_diffs: vec![AccountStorageDiff { address, diffs: Vec::new() }],
            new_codes: Vec::new(),
        };

        let (_, state) = decode_storage_diff_exact(&alloy_rlp::encode(diff)).unwrap();
        assert!(state.storages[&address].wiped);
        assert!(state.storages[&address].storage.is_empty());
        assert!(state.accounts[&address].is_some());
    }

    #[test]
    fn create_then_destroy_emits_no_state_diff_entries() {
        use reth_revm::db::{AccountStatus, BundleAccount, BundleState};

        let mut bundle = BundleState::default();
        bundle.state.insert(
            Address::repeat_byte(0x77),
            BundleAccount::new(None, None, Default::default(), AccountStatus::Destroyed),
        );
        let diff =
            build_storage_diff(&bundle, B256::repeat_byte(0x01), B256::repeat_byte(0x02)).unwrap();
        assert!(diff.new_accounts.is_empty());
        assert!(diff.deleted_accounts.is_empty());
        assert!(diff.storage_diffs.is_empty());
        assert!(diff.new_codes.is_empty());
    }

    #[test]
    fn deleted_account_ignores_retained_intermediate_storage() {
        use reth_revm::{
            db::{
                AccountStatus, BundleAccount, BundleState,
                states::{StorageSlot, StorageWithOriginalValues},
            },
            state::AccountInfo,
        };

        let address = Address::repeat_byte(0x88);
        let mut storage = StorageWithOriginalValues::default();
        storage.insert(U256::from(1), StorageSlot::new_changed(U256::ZERO, U256::from(2)));
        let mut bundle = BundleState::default();
        bundle.state.insert(
            address,
            BundleAccount::new(
                Some(AccountInfo::default()),
                None,
                storage,
                AccountStatus::Destroyed,
            ),
        );

        let diff =
            build_storage_diff(&bundle, B256::repeat_byte(0x03), B256::repeat_byte(0x04)).unwrap();
        assert_eq!(diff.deleted_accounts, vec![hashed_address(address)]);
        assert!(diff.new_accounts.is_empty());
        assert!(diff.storage_diffs.is_empty());
        let (_, state) = decode_storage_diff_exact(&alloy_rlp::encode(diff)).unwrap();
        assert!(state.storages[&hashed_address(address)].wiped);
    }

    #[test]
    fn storage_clear_is_emitted_but_write_restore_is_omitted() {
        use reth_revm::{
            db::{
                AccountStatus, BundleAccount, BundleState,
                states::{StorageSlot, StorageWithOriginalValues},
            },
            state::AccountInfo,
        };

        let cleared_address = Address::repeat_byte(0x91);
        let restored_address = Address::repeat_byte(0x92);
        let info = AccountInfo::default();
        let mut cleared_storage = StorageWithOriginalValues::default();
        cleared_storage.insert(U256::from(1), StorageSlot::new_changed(U256::from(7), U256::ZERO));
        let mut restored_storage = StorageWithOriginalValues::default();
        restored_storage.insert(U256::from(2), StorageSlot::new(U256::from(8)));
        let mut bundle = BundleState::default();
        bundle.state.insert(
            cleared_address,
            BundleAccount::new(
                Some(info.clone()),
                Some(info.clone()),
                cleared_storage,
                AccountStatus::Changed,
            ),
        );
        bundle.state.insert(
            restored_address,
            BundleAccount::new(
                Some(info.clone()),
                Some(info),
                restored_storage,
                AccountStatus::Changed,
            ),
        );

        let diff =
            build_storage_diff(&bundle, B256::repeat_byte(0x05), B256::repeat_byte(0x06)).unwrap();
        assert_eq!(diff.new_accounts.len(), 1);
        assert_eq!(diff.new_accounts[0].address, hashed_address(cleared_address));
        assert_eq!(diff.storage_diffs.len(), 1);
        assert_eq!(diff.storage_diffs[0].diffs.len(), 1);
        assert_eq!(diff.storage_diffs[0].diffs[0].value, U256::ZERO);
    }

    #[test]
    fn destroy_recreate_emits_wipe_and_complete_final_storage() {
        use reth_revm::{
            db::{
                AccountStatus, BundleAccount, BundleState,
                states::{StorageSlot, StorageWithOriginalValues},
            },
            state::AccountInfo,
        };

        let address = Address::repeat_byte(0x93);
        let mut storage = StorageWithOriginalValues::default();
        storage.insert(U256::from(1), StorageSlot::new(U256::from(7)));
        storage.insert(U256::from(2), StorageSlot::new_changed(U256::ZERO, U256::from(8)));
        let mut bundle = BundleState::default();
        bundle.state.insert(
            address,
            BundleAccount::new(
                Some(AccountInfo::default()),
                Some(AccountInfo { nonce: 1, ..Default::default() }),
                storage,
                AccountStatus::DestroyedChanged,
            ),
        );

        let diff =
            build_storage_diff(&bundle, B256::repeat_byte(0x07), B256::repeat_byte(0x08)).unwrap();
        assert_eq!(diff.deleted_accounts, vec![hashed_address(address)]);
        assert_eq!(diff.new_accounts.len(), 1);
        assert_eq!(diff.storage_diffs.len(), 1);
        assert_eq!(diff.storage_diffs[0].diffs.len(), 2);
        let (_, state) = decode_storage_diff_exact(&alloy_rlp::encode(diff)).unwrap();
        assert!(state.storages[&hashed_address(address)].wiped);
        assert_eq!(state.storages[&hashed_address(address)].storage.len(), 2);
    }

    #[test]
    fn code_proof_rejects_missing_and_wrong_bytes_after_recreate() {
        use reth_revm::{
            db::{AccountStatus, BundleAccount, BundleState},
            state::{AccountInfo, Bytecode},
        };

        let address = Address::repeat_byte(0x94);
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let code_hash = keccak256(&code);
        let info = AccountInfo { code_hash, code: None, ..Default::default() };
        let mut bundle = BundleState::default();
        bundle.state.insert(
            address,
            BundleAccount::new(
                Some(info.clone()),
                Some(info),
                Default::default(),
                AccountStatus::DestroyedChanged,
            ),
        );

        assert!(
            build_storage_diff(&bundle, B256::repeat_byte(0x09), B256::repeat_byte(0x0a))
                .unwrap_err()
                .contains("missing bytecode")
        );
        bundle.contracts.insert(code_hash, Bytecode::new_raw(Bytes::from_static(&[0x00])));
        assert!(
            build_storage_diff(&bundle, B256::repeat_byte(0x09), B256::repeat_byte(0x0a))
                .unwrap_err()
                .contains("does not hash")
        );
        bundle.contracts.insert(code_hash, Bytecode::new_raw(code.clone()));
        let diff =
            build_storage_diff(&bundle, B256::repeat_byte(0x09), B256::repeat_byte(0x0a)).unwrap();
        assert_eq!(diff.new_codes, vec![NewCode { code_hash, code }]);
    }
}
