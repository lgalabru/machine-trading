//! What runs a strategy component, shared by the grader and by the runtime
//! that trades on mainnet (`docs/rfd/0004-strategies-on-mainnet.md`).
//!
//! [`host`] is the sandbox: Wasmtime, the `mtx:strategy` worlds, fuel and
//! memory limits. It reads the chain and sends raw transactions only
//! through what its embedder hands it ([`host::Reads`], [`host::RawSend`]).

mod error;
pub mod host;

pub use error::{Error, Result};
