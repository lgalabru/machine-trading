//! Shared logic for the mtx Solana trading benchmark.

pub mod actors;
pub mod carry;
pub mod chain;
pub mod compete;
mod error;
pub mod faults;
pub mod flow;
pub mod grade;
pub mod guard;
pub mod making;
pub mod offhours;
pub mod orders;
pub mod phoenix;
pub mod scenario;
pub mod session;
pub mod snapshot;
pub mod spot;
pub mod weekend;

pub use error::{Error, Result};
