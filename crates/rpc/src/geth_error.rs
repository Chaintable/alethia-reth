//! Geth-compatible opcode error capture for DeBank call traces.

use alethia_reth_evm::alloy::TaikoEvmContext;
use reth_evm::precompiles::{DynPrecompile, Precompile, PrecompileInput, PrecompilesMap};
use reth_revm::{
    Database, Inspector,
    bytecode::opcode::{self, OpCode},
    context_interface::Host,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, InstructionResult, Interpreter,
        interpreter::EthInterpreter,
        interpreter_types::{Jumps, LoopControl, RuntimeFlag},
    },
    precompile::{PrecompileHalt, PrecompileStatus},
    primitives::{Address, Bytes, hardfork::SpecId},
};
use std::{
    cmp::Ordering,
    collections::VecDeque,
    sync::{Arc, Mutex},
};

/// Maximum stack height enforced by Geth and REVM.
const GETH_STACK_LIMIT: usize = 1024;

/// BN254 base-field modulus used to distinguish Geth's two coordinate errors.
const BN254_FIELD_MODULUS: [u8; 32] = [
    0x30, 0x64, 0x4e, 0x72, 0xe1, 0x31, 0xa0, 0x29, 0xb8, 0x50, 0x45, 0xb6, 0x81, 0x81, 0x58, 0x5d,
    0x97, 0x81, 0x6a, 0x91, 0x68, 0x71, 0xca, 0x8d, 0x3c, 0x20, 0x8c, 0x16, 0xd8, 0x7c, 0xfd, 0x47,
];

/// A typed precompile halt captured before REVM reduces it to `PrecompileError`.
#[derive(Clone, Debug)]
struct CapturedPrecompileError {
    /// Address whose precompile implementation ran.
    address: Address,
    /// Exact input passed to the precompile.
    input: Bytes,
    /// Typed halt returned before the interpreter-level conversion.
    halt: PrecompileHalt,
}

/// Request-local recorder for typed precompile failures.
#[derive(Clone, Debug, Default)]
pub(crate) struct PrecompileErrorCapture {
    /// Failures awaiting their matching inspector `call_end`.
    errors: Arc<Mutex<VecDeque<CapturedPrecompileError>>>,
}

impl PrecompileErrorCapture {
    /// Wraps this replay EVM's precompiles without changing the shared EVM factory.
    pub(crate) fn install(&self, precompiles: &mut PrecompilesMap) {
        let capture = self.clone();
        precompiles.map_precompiles(move |_, precompile| {
            let capture = capture.clone();
            let precompile_id = precompile.precompile_id().clone();
            let supports_caching = precompile.supports_caching();
            let wrapper = move |input: PrecompileInput<'_>| {
                let address = input.bytecode_address;
                let data = input.data;
                let result = precompile.call(input);
                if let Ok(output) = &result &&
                    let PrecompileStatus::Halt(halt) = &output.status &&
                    !halt.is_oog()
                {
                    capture.push(CapturedPrecompileError {
                        address,
                        input: Bytes::copy_from_slice(data),
                        halt: halt.clone(),
                    });
                }
                result
            };
            if supports_caching {
                DynPrecompile::new(precompile_id, wrapper)
            } else {
                DynPrecompile::new_stateful(precompile_id, wrapper)
            }
        });
    }

    /// Appends one precompile halt in execution order.
    fn push(&self, error: CapturedPrecompileError) {
        self.errors.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push_back(error);
    }

    /// Removes the next precompile halt in execution order.
    fn pop(&self) -> Option<CapturedPrecompileError> {
        self.errors.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pop_front()
    }

    /// Clears transaction-local leftovers and returns how many were present.
    fn clear_and_count(&self) -> usize {
        let mut errors = self.errors.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = errors.len();
        errors.clear();
        count
    }
}

/// Exact error information collected for one active call or create frame.
#[derive(Debug)]
struct FrameCapture {
    /// Arena index assigned in the same call/create callback order as `TracingInspector`.
    node_index: usize,
    /// Geth-compatible error for the opcode that terminated this frame.
    exact_error: Option<String>,
}

/// Pre-execution information for the opcode currently being inspected.
#[derive(Debug)]
struct PendingStep {
    /// Raw opcode byte, retained for invariant diagnostics.
    opcode: u8,
    /// Error Geth would return before charging opcode gas.
    precheck_error: Option<String>,
    /// Exact error for a direct cold-target gas failure, pending confirmation by `step_end`.
    dynamic_error: Option<String>,
}

/// Sidecar inspector that preserves error context discarded by `InstructionResult`.
///
/// The inspector mirrors `TracingInspector` frame insertion order. It performs Geth's fork and
/// stack checks in `step`, before REVM charges static gas, then binds the exact error to the active
/// node in `step_end`.
#[derive(Debug, Default)]
pub(crate) struct GethErrorInspector {
    /// Active frame stack in EVM nesting order.
    frames: Vec<FrameCapture>,
    /// Completed errors indexed by `CallTraceNode::idx`.
    errors: Vec<Option<String>>,
    /// Opcode awaiting its matching `step_end` callback.
    pending_step: Option<PendingStep>,
    /// Typed precompile failures shared with this replay EVM's precompile wrappers.
    precompile_errors: PrecompileErrorCapture,
    /// First invariant violation observed during the transaction.
    fault: Option<String>,
}

impl GethErrorInspector {
    /// Creates a sidecar bound to the request-local precompile error recorder.
    pub(crate) fn with_precompile_errors(precompile_errors: PrecompileErrorCapture) -> Self {
        Self { precompile_errors, ..Default::default() }
    }

    /// Records the first invariant violation while allowing execution to finish safely.
    fn record_fault(&mut self, detail: impl Into<String>) {
        if self.fault.is_none() {
            self.fault = Some(detail.into());
        }
    }

    /// Starts a frame using the next trace-arena index.
    fn start_frame(&mut self) {
        if self.pending_step.is_some() {
            self.record_fault("call/create frame started before the previous opcode ended");
        }
        let node_index = self.errors.len();
        self.errors.push(None);
        self.frames.push(FrameCapture { node_index, exact_error: None });
    }

    /// Captures Geth's pre-gas opcode validation for the active frame.
    fn inspect_step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        dynamic_error: impl FnOnce(&Interpreter<EthInterpreter>) -> Option<String>,
    ) {
        if self.frames.is_empty() {
            self.record_fault("opcode step observed without an active call/create frame");
            return;
        }
        if self.pending_step.is_some() {
            self.record_fault("opcode step observed before the previous step_end");
            return;
        }

        let opcode = interp.bytecode.opcode();
        let precheck_error =
            geth_precheck_error(opcode, interp.runtime_flag.spec_id(), interp.stack.len());
        self.pending_step = Some(PendingStep {
            opcode,
            dynamic_error: precheck_error.is_none().then(|| dynamic_error(interp)).flatten(),
            precheck_error,
        });
    }

    /// Resolves the current opcode and attaches any required exact error to its frame.
    fn inspect_step_end(&mut self, interp: &mut Interpreter<EthInterpreter>) {
        let Some(pending) = self.pending_step.take() else {
            self.record_fault("step_end observed without a matching opcode step");
            return;
        };
        let result = interp.bytecode.instruction_result();

        let exact_error = if let Some(exact_error) = pending.precheck_error {
            Some(exact_error)
        } else if result == Some(InstructionResult::OutOfGas) {
            pending.dynamic_error
        } else {
            None
        };

        if let Some(exact_error) = exact_error {
            if !result.is_some_and(InstructionResult::is_error) {
                self.record_fault(format!(
                    "opcode {:#x} failed Geth precheck but did not halt the frame",
                    pending.opcode
                ));
                return;
            }
            let Some(frame) = self.frames.last_mut() else {
                self.record_fault("opcode step_end observed without an active frame");
                return;
            };
            let node_index = frame.node_index;
            let duplicate = frame.exact_error.replace(exact_error).is_some();
            if duplicate {
                self.record_fault(format!(
                    "trace node {} recorded more than one terminal opcode error",
                    node_index
                ));
            }
        } else if result.is_some_and(requires_exact_error) {
            self.record_fault(format!(
                "opcode {:#x} lost context required for exact Geth error text",
                pending.opcode
            ));
        }
    }

    /// Attaches a typed precompile halt to the call frame that invoked it.
    fn inspect_precompile_error(&mut self, bytecode_address: Address) {
        let Some(captured) = self.precompile_errors.pop() else {
            self.record_fault(format!(
                "precompile frame {bytecode_address} ended without a typed halt"
            ));
            return;
        };
        if captured.address != bytecode_address {
            self.record_fault(format!(
                "captured precompile {} does not match ending frame {bytecode_address}",
                captured.address
            ));
            return;
        }
        let exact_error = match geth_precompile_error(&captured) {
            Ok(error) => error,
            Err(error) => {
                self.record_fault(error);
                return;
            }
        };
        let Some(frame) = self.frames.last_mut() else {
            self.record_fault("precompile error observed without an active call frame");
            return;
        };
        let node_index = frame.node_index;
        let duplicate = frame.exact_error.replace(exact_error).is_some();
        if duplicate {
            self.record_fault(format!(
                "trace node {node_index} recorded more than one terminal error"
            ));
        }
    }

    /// Ends the active frame and stores its error by trace-arena index.
    fn end_frame(&mut self, result: InstructionResult) {
        if self.pending_step.is_some() {
            self.record_fault("call/create frame ended before the current opcode step_end");
            self.pending_step = None;
        }
        let Some(frame) = self.frames.pop() else {
            self.record_fault("call/create frame ended without a matching start");
            return;
        };
        if frame.exact_error.is_some() && !result.is_error() {
            self.record_fault(format!(
                "trace node {} captured an opcode error but ended with {result:?}",
                frame.node_index
            ));
        }
        if frame.exact_error.is_none() && requires_exact_error(result) {
            self.record_fault(format!(
                "trace node {} ended with {result:?} without exact Geth error context",
                frame.node_index
            ));
        }
        self.errors[frame.node_index] = frame.exact_error;
    }

    /// Validates and consumes one transaction's node-indexed errors.
    ///
    /// State is reset on both success and failure so a subsequent transaction cannot inherit
    /// partial frame or opcode context.
    pub(crate) fn finish_transaction(
        &mut self,
        expected_nodes: usize,
    ) -> Result<Vec<Option<String>>, String> {
        let mut fault = self.fault.take();
        if fault.is_none() && self.pending_step.is_some() {
            fault = Some("transaction ended with an unfinished opcode step".into());
        }
        if fault.is_none() && !self.frames.is_empty() {
            fault = Some(format!(
                "transaction ended with {} active call/create frames",
                self.frames.len()
            ));
        }
        if fault.is_none() && self.errors.len() != expected_nodes {
            fault = Some(format!(
                "Geth error sidecar recorded {} nodes, trace arena recorded {expected_nodes}",
                self.errors.len()
            ));
        }
        let unmatched_precompile_errors = self.precompile_errors.clear_and_count();
        if fault.is_none() && unmatched_precompile_errors != 0 {
            fault = Some(format!(
                "transaction ended with {unmatched_precompile_errors} unmatched precompile errors"
            ));
        }

        let errors = std::mem::take(&mut self.errors);
        self.frames.clear();
        self.pending_step = None;
        match fault {
            Some(error) => Err(error),
            None => Ok(errors),
        }
    }
}

impl<DB: Database> Inspector<TaikoEvmContext<DB>, EthInterpreter> for GethErrorInspector {
    /// Captures fork and stack checks before REVM charges static opcode gas.
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        let warm_cost = context.gas_params().warm_storage_read_cost();
        let cold_additional_cost = context.gas_params().cold_account_additional_cost();
        self.inspect_step(interp, |interp| {
            geth_dynamic_call_error(interp, warm_cost, cold_additional_cost, |target| {
                journal_address_is_cold(context, target)
            })
        });
    }

    /// Binds a terminal precheck error to the active trace node.
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut TaikoEvmContext<DB>,
    ) {
        self.inspect_step_end(interp);
    }

    /// Mirrors `TracingInspector` call-node insertion order.
    fn call(
        &mut self,
        _context: &mut TaikoEvmContext<DB>,
        _inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.start_frame();
        None
    }

    /// Completes the active call-node error record.
    fn call_end(
        &mut self,
        _context: &mut TaikoEvmContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if outcome.result.result == InstructionResult::PrecompileError {
            self.inspect_precompile_error(inputs.bytecode_address);
        }
        self.end_frame(outcome.result.result);
    }

    /// Mirrors `TracingInspector` create-node insertion order.
    fn create(
        &mut self,
        _context: &mut TaikoEvmContext<DB>,
        _inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.start_frame();
        None
    }

    /// Completes the active create-node error record.
    fn create_end(
        &mut self,
        _context: &mut TaikoEvmContext<DB>,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.end_frame(outcome.result.result);
    }
}

/// Returns the nested Geth error for a direct cold-target charge that cannot be paid.
///
/// This is deliberately conservative: REVM must still report `OutOfGas` in `step_end`. It covers
/// the primary EIP-2929 target charge, including after EIP-7702 activation, but not a later
/// delegation-target charge.
fn geth_dynamic_call_error(
    interp: &Interpreter<EthInterpreter>,
    warm_cost: u64,
    cold_additional_cost: u64,
    target_is_cold: impl FnOnce(Address) -> bool,
) -> Option<String> {
    if !interp.runtime_flag.spec_id().is_enabled_in(SpecId::BERLIN) ||
        !matches!(
            interp.bytecode.opcode(),
            opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL
        )
    {
        return None;
    }

    let remaining_after_static = interp.gas.remaining().checked_sub(warm_cost)?;
    if remaining_after_static >= cold_additional_cost {
        return None;
    }
    let target = Address::from_word(interp.stack.peek(1).ok()?.into());
    target_is_cold(target).then(|| "out of gas: out of gas".into())
}

/// Reproduces REVM's read-only account warmth check without loading or mutating journal state.
fn journal_address_is_cold<DB: Database>(context: &TaikoEvmContext<DB>, address: Address) -> bool {
    let journal = &context.journaled_state;
    journal.warm_addresses.is_cold(&address) &&
        journal
            .state
            .get(&address)
            .is_none_or(|account| account.is_cold_transaction_id(journal.transaction_id))
}

/// Returns whether a final result requires opcode-local context for exact Geth text.
pub(crate) const fn requires_exact_error(result: InstructionResult) -> bool {
    matches!(
        result,
        InstructionResult::OpcodeNotFound |
            InstructionResult::InvalidFEOpcode |
            InstructionResult::NotActivated |
            InstructionResult::StackUnderflow |
            InstructionResult::StackOverflow |
            InstructionResult::PrecompileError
    )
}

/// Converts a captured REVM precompile halt into the text emitted by Taiko-Geth.
fn geth_precompile_error(error: &CapturedPrecompileError) -> Result<String, String> {
    let text = match error.halt {
        PrecompileHalt::Blake2WrongLength => "invalid input length",
        PrecompileHalt::Blake2WrongFinalIndicatorFlag => "invalid final flag",
        PrecompileHalt::ModexpEip7823LimitSize => {
            "one or more of base/exponent/modulus length exceeded 1024 bytes"
        }
        PrecompileHalt::Bn254FieldPointNotAMember => {
            return geth_bn254_field_error(error.address, &error.input);
        }
        PrecompileHalt::Bn254AffineGFailedToCreate => "bn256: malformed point",
        PrecompileHalt::Bn254PairLength => "bad elliptic curve pairing size",
        PrecompileHalt::BlobInvalidInputLength => "invalid input length",
        PrecompileHalt::BlobMismatchedVersion => "mismatched versioned hash",
        // REVM's c-kzg backend has already discarded Geth's backend-specific suffix here. Keep
        // Geth's stable outer error so one invalid proof cannot wedge the block pipeline.
        PrecompileHalt::BlobVerifyKzgProofFailed => "error verifying kzg proof",
        PrecompileHalt::NonCanonicalFp if is_bls_precompile(error.address) => {
            "invalid fp.Element encoding"
        }
        PrecompileHalt::Bls12381G1NotOnCurve | PrecompileHalt::Bls12381G2NotOnCurve => {
            "invalid point: not on curve"
        }
        PrecompileHalt::Bls12381G1NotInSubgroup => "g1 point is not on correct subgroup",
        PrecompileHalt::Bls12381G2NotInSubgroup => "g2 point is not on correct subgroup",
        PrecompileHalt::Bls12381ScalarInputLength |
        PrecompileHalt::Bls12381G1AddInputLength |
        PrecompileHalt::Bls12381G1MsmInputLength |
        PrecompileHalt::Bls12381G2AddInputLength |
        PrecompileHalt::Bls12381G2MsmInputLength |
        PrecompileHalt::Bls12381PairingInputLength |
        PrecompileHalt::Bls12381MapFpToG1InputLength |
        PrecompileHalt::Bls12381MapFp2ToG2InputLength => "invalid input length",
        PrecompileHalt::Bls12381FpPaddingInvalid => "invalid field element top bytes",
        PrecompileHalt::Bls12381FpPaddingLength => "invalid field element length",
        PrecompileHalt::Bls12381G1PaddingLength => "invalid g1 point length",
        PrecompileHalt::Bls12381G2PaddingLength => "invalid g2 point length",
        PrecompileHalt::KzgInvalidG1Point |
        PrecompileHalt::KzgG1PointNotOnCurve |
        PrecompileHalt::KzgG1PointNotInSubgroup |
        PrecompileHalt::KzgInvalidInputLength |
        PrecompileHalt::NonCanonicalFp |
        PrecompileHalt::ModexpExpOverflow |
        PrecompileHalt::ModexpBaseOverflow |
        PrecompileHalt::ModexpModOverflow |
        PrecompileHalt::Secp256k1RecoverFailed |
        PrecompileHalt::Other(_) |
        PrecompileHalt::OutOfGas => {
            return Err(format!(
                "precompile {} ended with {:?}, whose exact Geth error is unavailable",
                error.address, error.halt
            ));
        }
    };
    Ok(text.into())
}

/// Returns whether `address` is one of the Osaka BLS12-381 precompiles.
fn is_bls_precompile(address: Address) -> bool {
    (0x0b..=0x11).any(|suffix| address == Address::with_last_byte(suffix))
}

/// Distinguishes the two field-range errors emitted by Geth's Cloudflare BN254 implementation.
fn geth_bn254_field_error(address: Address, input: &[u8]) -> Result<String, String> {
    let coordinate_bytes = if address == Address::with_last_byte(0x06) {
        128
    } else if address == Address::with_last_byte(0x07) {
        64
    } else if address == Address::with_last_byte(0x08) {
        input.len()
    } else {
        return Err(format!("BN254 field error reported by unexpected precompile {address}"));
    };

    for offset in (0..coordinate_bytes).step_by(32) {
        let mut coordinate = [0u8; 32];
        if offset < input.len() {
            let end = input.len().min(offset + coordinate.len());
            coordinate[..end - offset].copy_from_slice(&input[offset..end]);
        }
        match coordinate.cmp(&BN254_FIELD_MODULUS) {
            Ordering::Greater => return Ok("bn256: coordinate exceeds modulus".into()),
            Ordering::Equal => return Ok("bn256: coordinate equals modulus".into()),
            Ordering::Less => {}
        }
    }
    Err(format!("BN254 field error for precompile {address} had no out-of-range coordinate"))
}

/// Reproduces the checks Geth performs before charging an opcode's static gas.
fn geth_precheck_error(opcode_byte: u8, spec: SpecId, stack_len: usize) -> Option<String> {
    if opcode_byte == opcode::INVALID {
        return Some("invalid opcode: INVALID".into());
    }
    let Some(opcode) = OpCode::new(opcode_byte) else {
        return Some(format!("invalid opcode: opcode {opcode_byte:#x} not defined"));
    };
    if !opcode_is_active(opcode_byte, spec) {
        return Some(format!("invalid opcode: {}", opcode.as_str()));
    }

    let required = usize::from(opcode.inputs());
    if stack_len < required {
        return Some(format!("stack underflow ({stack_len} <=> {required})"));
    }
    let maximum = GETH_STACK_LIMIT + required - usize::from(opcode.outputs());
    (stack_len > maximum).then(|| format!("stack limit reached {stack_len} ({maximum})"))
}

/// Returns whether an opcode exists in the active Geth jump table for `spec`.
///
/// This table is pinned to the conditional opcodes in the lockfile's REVM 35 interpreter. Taiko
/// maps all historical forks through Shasta to Shanghai and Unzen to Osaka, but older and future
/// minima are retained so known post-Osaka opcodes remain invalid instead of receiving stack
/// validation.
fn opcode_is_active(opcode_byte: u8, spec: SpecId) -> bool {
    let minimum = match opcode_byte {
        opcode::DELEGATECALL => SpecId::HOMESTEAD,
        opcode::RETURNDATASIZE | opcode::RETURNDATACOPY | opcode::STATICCALL | opcode::REVERT => {
            SpecId::BYZANTIUM
        }
        opcode::SHL | opcode::SHR | opcode::SAR | opcode::EXTCODEHASH => SpecId::CONSTANTINOPLE,
        opcode::CREATE2 => SpecId::PETERSBURG,
        opcode::CHAINID | opcode::SELFBALANCE => SpecId::ISTANBUL,
        opcode::BASEFEE => SpecId::LONDON,
        opcode::PUSH0 => SpecId::SHANGHAI,
        opcode::BLOBHASH | opcode::BLOBBASEFEE | opcode::TLOAD | opcode::TSTORE | opcode::MCOPY => {
            SpecId::CANCUN
        }
        opcode::CLZ => SpecId::OSAKA,
        opcode::SLOTNUM | opcode::DUPN | opcode::SWAPN | opcode::EXCHANGE => SpecId::AMSTERDAM,
        _ => SpecId::FRONTIER,
    };
    spec.is_enabled_in(minimum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, U256};
    use reth_revm::{
        bytecode::Bytecode,
        interpreter::{
            InputsImpl, SharedMemory, host::DummyHost, instructions::instruction_table,
            interpreter::ExtBytecode,
        },
    };

    fn single_opcode_interpreter(
        code: &[u8],
        spec: SpecId,
        gas_limit: u64,
        stack_len: usize,
    ) -> Interpreter<EthInterpreter> {
        let bytecode = Bytecode::new_legacy(Bytes::copy_from_slice(code));
        let mut interpreter = Interpreter::<EthInterpreter>::new(
            SharedMemory::new(),
            ExtBytecode::new(bytecode),
            InputsImpl::default(),
            false,
            spec,
            gas_limit,
        );
        for _ in 0..stack_len {
            assert!(interpreter.stack.push(U256::ZERO));
        }
        interpreter
    }

    fn inspect_single_opcode(
        code: &[u8],
        spec: SpecId,
        gas_limit: u64,
        stack_len: usize,
    ) -> (InstructionResult, String) {
        let mut interpreter = single_opcode_interpreter(code, spec, gas_limit, stack_len);
        let mut inspector = GethErrorInspector::default();
        inspector.start_frame();
        inspector.inspect_step(&mut interpreter, |_| None);
        let mut host = DummyHost::new(spec);
        interpreter.step(&instruction_table::<EthInterpreter, DummyHost>(), &mut host);
        inspector.inspect_step_end(&mut interpreter);
        let result = interpreter.bytecode.instruction_result().expect("terminal opcode result");
        inspector.end_frame(result);
        let errors = inspector.finish_transaction(1).expect("valid sidecar state");
        (result, errors[0].clone().expect("exact opcode error"))
    }

    #[test]
    fn unknown_opcode_uses_geth_hex_text() {
        let (result, error) = inspect_single_opcode(&[0xaa], SpecId::SHANGHAI, 1_000, 0);
        assert_eq!(result, InstructionResult::OpcodeNotFound);
        assert_eq!(error, "invalid opcode: opcode 0xaa not defined");
    }

    #[test]
    fn invalid_fe_uses_geth_opcode_name() {
        let (result, error) = inspect_single_opcode(&[opcode::INVALID], SpecId::SHANGHAI, 1_000, 0);
        assert_eq!(result, InstructionResult::InvalidFEOpcode);
        assert_eq!(error, "invalid opcode: INVALID");
    }

    #[test]
    fn inactive_opcode_wins_before_stack_and_activates_at_the_fork() {
        let (inactive_result, inactive_error) =
            inspect_single_opcode(&[opcode::TLOAD], SpecId::SHANGHAI, 1_000, 0);
        assert_eq!(inactive_result, InstructionResult::NotActivated);
        assert_eq!(inactive_error, "invalid opcode: TLOAD");

        let (active_result, active_error) =
            inspect_single_opcode(&[opcode::TLOAD], SpecId::OSAKA, 1_000, 0);
        assert_eq!(active_result, InstructionResult::StackUnderflow);
        assert_eq!(active_error, "stack underflow (0 <=> 1)");
    }

    #[test]
    fn stack_underflow_includes_actual_and_required_heights() {
        let (result, error) = inspect_single_opcode(&[opcode::ADD], SpecId::SHANGHAI, 1_000, 0);
        assert_eq!(result, InstructionResult::StackUnderflow);
        assert_eq!(error, "stack underflow (0 <=> 2)");
    }

    #[test]
    fn geth_stack_check_precedes_revms_static_gas_charge() {
        let (result, error) = inspect_single_opcode(&[opcode::ADD], SpecId::SHANGHAI, 0, 0);
        assert_eq!(result, InstructionResult::OutOfGas);
        assert_eq!(error, "stack underflow (0 <=> 2)");

        let mut call = single_opcode_interpreter(&[opcode::CALL], SpecId::SHANGHAI, 2_599, 7);
        assert_eq!(
            geth_dynamic_call_error(&call, 100, 2_500, |_| true).as_deref(),
            Some("out of gas: out of gas")
        );
        assert!(geth_dynamic_call_error(&call, 100, 2_500, |_| false).is_none());

        let mut inspector = GethErrorInspector::default();
        inspector.start_frame();
        inspector
            .inspect_step(&mut call, |call| geth_dynamic_call_error(call, 100, 2_500, |_| true));
        call.halt(InstructionResult::OutOfGas);
        inspector.inspect_step_end(&mut call);
        let result = call.bytecode.instruction_result().expect("terminal call result");
        assert_eq!(result, InstructionResult::OutOfGas);
        inspector.end_frame(result);
        assert_eq!(
            inspector.finish_transaction(1).unwrap(),
            vec![Some("out of gas: out of gas".into())]
        );

        for gas_limit in [99, 2_600] {
            let call = single_opcode_interpreter(&[opcode::CALL], SpecId::SHANGHAI, gas_limit, 7);
            assert!(geth_dynamic_call_error(&call, 100, 2_500, |_| true).is_none());
        }
    }

    #[test]
    fn stack_overflow_uses_opcode_specific_geth_limit() {
        let (result, error) =
            inspect_single_opcode(&[opcode::PUSH1, 0], SpecId::SHANGHAI, 1_000, 1024);
        assert_eq!(result, InstructionResult::StackOverflow);
        assert_eq!(error, "stack limit reached 1024 (1023)");
    }

    #[test]
    fn transaction_finish_fails_closed_and_resets_state() {
        let mut inspector = GethErrorInspector::default();
        inspector.start_frame();
        assert!(inspector.finish_transaction(1).unwrap_err().contains("active"));
        assert_eq!(inspector.finish_transaction(0).unwrap(), Vec::<Option<String>>::new());
    }
}
