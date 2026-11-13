//! Shared logic for the mtx Solana trading benchmark.

pub mod carry;
pub mod chain;
pub mod compete;
pub mod crank;
mod error;
pub mod faults;
pub mod flow;
pub mod generators;
pub mod grade;
pub mod guard;
pub mod keys;
pub mod making;
pub mod market;
pub mod offhours;
pub mod oracles;
pub mod orders;
pub mod phoenix;
pub mod raw;
pub mod recipe;
pub mod replay;
pub mod scenario;
pub mod scenario_v2;
pub mod session;
pub mod simulated;
pub mod snapshot;
pub mod spot;
pub mod wasm;
pub mod weekend;

pub use error::{Error, Result};
