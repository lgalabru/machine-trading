//! Shared logic for the mth Solana trading benchmark.

pub mod chain;
mod error;
pub mod flow;
pub mod grade;
pub mod guard;
pub mod making;
pub mod phoenix;
pub mod scenario;
pub mod session;
pub mod snapshot;

pub use error::{Error, Result};
