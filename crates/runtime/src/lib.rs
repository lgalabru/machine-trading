//! What runs a strategy component, shared by the grader and by the runtime
//! that trades on mainnet (`docs/rfd/0004-strategies-on-mainnet.md`).
//!
//! - [`host`] is the sandbox: Wasmtime, the `mtx:strategy` worlds, fuel and
//!   memory limits. It reads the chain and sends raw transactions only
//!   through what its embedder hands it ([`host::Reads`],
//!   [`host::RawSend`]).
//! - [`desk`] plays a session with it: reads the market, calls `step`,
//!   plans the sheet and sends it through [`venue`]'s seats, after
//!   [`taking`]'s check.
//! - [`reads`] is how a host makes its own reads of the chain.

pub mod desk;
mod error;
pub mod host;
pub mod reads;
pub mod taking;
pub mod venue;

pub use error::{Error, Result};
