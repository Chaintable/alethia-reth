#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko RPC namespace extensions for engine and `eth` APIs.
/// DeBank block-file wire types and deterministic formatter.
pub mod debank;
/// Proof-history backed `debug_` namespace witness overrides.
pub mod debug;
/// Engine API extensions and validator/builder wiring.
pub mod engine;
/// Taiko `eth` namespace extensions and custom RPC methods.
pub mod eth;
/// Geth-compatible opcode error capture for DeBank traces.
mod geth_error;
/// Proof-history backed state provider factory for RPC witness generation.
pub mod proof_state;
/// `trace_debankBlock` RPC implementation.
pub mod trace;
/// Transport-specific registration helpers for custom RPC methods.
pub mod transport;
