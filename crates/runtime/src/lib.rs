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
//! - `live` (feature `live`) is the runtime that runs strategies against a
//!   live chain, fed by a geyser plugin.

pub mod desk;
mod error;
pub mod host;
#[cfg(feature = "live")]
pub mod live;
pub mod reads;
pub mod taking;
pub mod venue;

pub use error::{Error, Result};
