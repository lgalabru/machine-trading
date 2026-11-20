//! Coding agents that speak ACP, in Rust: Claude Code ([`claude`], `mtx
//! claude-acp`) and Codex ([`codex`], `mtx codex-acp`). Each drives the
//! harness's own command (`claude`, `codex app-server`), so mtx needs no
//! Node adapter to talk to either.
//!
//! Imported from Axel (<https://github.com/lgalabru/axel>, `crates/acp` at
//! 78cec51, Apache-2.0: see `LICENSE`), where they run as `axel claude-acp`
//! and `axel codex-acp`. The code is Axel's, laid out as it is there and
//! kept as close to it as it builds here, so that a later import is a copy:
//! the `_axel/*` notifications and `_meta.axel` keep their names, and what
//! the comments call Axel's client or session host is, here, mtx's ACP
//! client (`mtx-cli`'s `acp.rs`). What changed:
//!
//! - the translated strings are plain English ([`t!`]);
//! - a session's effort is read from the request ([`meta_effort`]) without
//!   Axel's store types;
//! - Axel's price table and plan windows are copied beside the agents
//!   (`pricing.rs`, and `LimitWindow` in `usage.rs`).

/// Axel translates these; here they are the English they are written in.
macro_rules! t {
    ($key:literal) => {
        $key
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        format!($key, $($name = $value),+)
    };
}

mod agent_rpc;
pub mod claude;
pub mod codex;
pub mod pricing;
pub mod usage;

use serde_json::Value;

/// The effort a session asks for (`_meta.axel.effort`), when it is one of
/// the five levels: low, medium, high, xhigh, max. Claude Code takes all
/// five; Codex stops at xhigh.
fn meta_effort(params: &Value) -> Option<&str> {
    params
        .pointer("/_meta/axel/effort")?
        .as_str()
        .filter(|effort| matches!(*effort, "low" | "medium" | "high" | "xhigh" | "max"))
}
