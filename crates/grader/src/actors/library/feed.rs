//! A house model's feed: calls on the market's next move, published on
//! chain through two gateways, one of which reports the opposite side.
//!
//! The model sees the scenario's level `horizon_slots` ahead and, while it
//! is more than `edge_bps` from the book's price, calls its way at
//! `calls_per_minute`, no two calls nearer than `min_gap_slots`. Its view
//! errs as an informed trader's does ([`Informed`](super::Informed)): for
//! one horizon in `1 - hit_rate` of them it is wrong outright.
//!
//! Each call goes out through one of two gateways, drawn at random per
//! call, as a memo transaction its wallet signs (SPL Memo), which anyone
//! reading the wallet's signatures sees with its text:
//!
//! ```text
//! {"call":3,"side":"buy","horizon_slots":160,"confidence":0.71}
//! ```
//!
//! One gateway reports the side the model would take (taker side); the
//! other reports the side a maker would be filled on, the opposite. Which
//! is which is drawn per session from the session's seed: the two wallets'
//! identities, told to the informed bot as the roster kinds
//! [`TAKER_SIDE`] and [`MAKER_SIDE`]. Calls carry the same horizon, pace and
//! confidence whichever gateway sends them, so only how the market moves
//! after them tells the gateways apart.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{Instruction, Transaction};

use super::{Actions, ActorTally, Rng, Window, finite_positive, traders::View, within};
use crate::actors::{Actor, ActorsReport, Moment};

/// The roster kinds of the gateway that reports the model's own side, and
/// of the one that reports the opposite.
pub const TAKER_SIDE: &str = "gateway-taker-side";
pub const MAKER_SIDE: &str = "gateway-maker-side";

/// How many gateways carry the feed.
pub const GATEWAYS: u32 = 2;

/// SPL Memo, which the session's chain runs.
pub const MEMO_PROGRAM: Pubkey = Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// Lamports each gateway wallet holds for its fees.
pub const GATEWAY_LAMPORTS: u64 = 1_000_000_000;

fn most() -> f64 {
    0.85
}

/// A feed's settings, as a scenario lists them (`actors.feed`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Feed {
    pub horizon_slots: u64,
    pub edge_bps: f64,
    #[serde(default = "most")]
    pub hit_rate: f64,
    /// Calls a minute while the move ahead is past the edge.
    pub calls_per_minute: f64,
    pub min_gap_slots: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl Feed {
    pub fn validate(&self) -> Result<(), String> {
        if self.horizon_slots == 0 {
            return Err("horizon_slots must be at least 1".into());
        }
        finite_positive("edge_bps", self.edge_bps)?;
        within("hit_rate", self.hit_rate, 0.5, 1.0)?;
        finite_positive("calls_per_minute", self.calls_per_minute)
    }
}

/// One call, as its memo states it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Call {
    pub call: u32,
    pub side: Side,
    pub horizon_slots: u64,
    pub confidence: f64,
}

/// What a call's view says before a gateway reports it: the side, and the
/// confidence, from how far past the edge the move it sees is.
fn confidence(edge_bps: f64, ahead_bps: f64) -> f64 {
    let past = 1.0 - edge_bps / ahead_bps.abs().max(edge_bps);
    ((0.5 + 0.45 * past) * 100.0).round() / 100.0
}

pub(super) struct FeedActor {
    spec: Feed,
    gateways: Vec<Arc<Keypair>>,
    /// The gateway that reports the opposite side.
    maker_side: usize,
    rpc_url: String,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    rng: Rng,
    view: View,
    last: Option<u64>,
    last_call: Option<u64>,
    calls: u32,
    published: Vec<u64>,
    failed: u64,
}

impl FeedActor {
    pub fn new(
        spec: Feed,
        gateways: Vec<Arc<Keypair>>,
        rpc_url: String,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
        mut rng: Rng,
    ) -> Self {
        let maker_side = rng.below(gateways.len());
        let view = View::new(spec.horizon_slots, spec.edge_bps, spec.hit_rate, 0.0);
        Self {
            spec,
            gateways,
            maker_side,
            rpc_url,
            path,
            rng,
            view,
            last: None,
            last_call: None,
            calls: 0,
            published: vec![0; GATEWAYS as usize],
            failed: 0,
        }
    }

    async fn publish(&mut self, gateway: usize, call: &Call) -> bool {
        let Ok(text) = serde_json::to_string(call) else {
            return false;
        };
        let signer = self.gateways[gateway].clone();
        let url = self.rpc_url.clone();
        let sent = tokio::task::spawn_blocking(move || {
            let rpc = mtx_kit::rpc_client_with_timeout(url, crate::session::INTERNAL_RPC_TIMEOUT);
            let blockhash = rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
            let memo = Instruction {
                program_id: MEMO_PROGRAM,
                accounts: Vec::new(),
                data: text.into_bytes(),
            };
            let tx = Transaction::new_signed_with_payer(
                &[memo],
                Some(&signer.pubkey()),
                &[signer.as_ref()],
                blockhash,
            );
            rpc.send_and_confirm_transaction(&tx)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await;
        let error = match sent {
            Ok(Ok(())) => return true,
            Ok(Err(error)) => error,
            Err(error) => error.to_string(),
        };
        tracing::warn!(%error, "a feed gateway's call didn't land");
        false
    }
}

#[async_trait]
impl Actor for FeedActor {
    async fn before_blocks(&mut self, last: &Moment) {
        let slots = last.at - self.last.unwrap_or(0);
        self.last = Some(last.at);
        if !Window::holds(&self.spec.active, last.at) {
            return;
        }
        let ahead = (self.path)(last.at + self.spec.horizon_slots).book_usd;
        let ahead_bps = (ahead / last.book_usd - 1.0) * 10_000.0;
        // The view is drawn every step, so its right or wrong lasts a
        // horizon whether or not a call goes out.
        let Some(side) = self.view.side(&mut self.rng, last.at, ahead_bps) else {
            return;
        };
        if self
            .last_call
            .is_some_and(|at| last.at < at + self.spec.min_gap_slots)
        {
            return;
        }
        let per_slot = self.spec.calls_per_minute * crate::scenario::SLOT_SECONDS / 60.0;
        if !self.rng.chance(1.0 - (-per_slot * slots as f64).exp()) {
            return;
        }
        let gateway = self.rng.below(self.gateways.len());
        self.calls += 1;
        self.last_call = Some(last.at);
        let call = Call {
            call: self.calls,
            side: if gateway == self.maker_side {
                side.opposite()
            } else {
                side
            },
            horizon_slots: self.spec.horizon_slots,
            confidence: confidence(self.spec.edge_bps, ahead_bps),
        };
        if self.publish(gateway, &call).await {
            self.published[gateway] += 1;
        } else {
            self.failed += 1;
        }
    }

    fn roster(&self) -> Vec<(String, Vec<String>)> {
        self.gateways
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let kind = if i == self.maker_side {
                    MAKER_SIDE
                } else {
                    TAKER_SIDE
                };
                (kind.to_string(), vec![g.pubkey().to_string()])
            })
            .collect()
    }

    /// Into the library's report, which the library's reporter has written
    /// by then: the feed's calls, each gateway's, and those that failed.
    async fn report(&mut self, report: &mut ActorsReport) {
        let Some(library) = report.library.as_mut() else {
            return;
        };
        let mut counts = std::collections::BTreeMap::new();
        counts.insert("calls".to_string(), u64::from(self.calls));
        for (i, published) in self.published.iter().enumerate() {
            counts.insert(format!("gateway_{i}"), *published);
        }
        let failed = self.failed as usize;
        library.actions.failed += failed;
        library.actors.push(ActorTally {
            kind: "feed".into(),
            wallets: self.gateways.iter().map(|g| g.pubkey().to_string()).collect(),
            actions: Actions {
                failed,
                ..Actions::default()
            },
            counts,
            ..ActorTally::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_grows_with_the_move_past_the_edge() {
        assert_eq!(confidence(20.0, 20.0), 0.5);
        assert_eq!(confidence(20.0, -10.0), 0.5);
        assert!(confidence(20.0, 40.0) > confidence(20.0, 25.0));
        assert!(confidence(20.0, 1e9) <= 0.95);
    }

    #[test]
    fn a_call_reads_as_the_memo_states_it() {
        let call = Call {
            call: 3,
            side: Side::Bid,
            horizon_slots: 160,
            confidence: 0.71,
        };
        assert_eq!(
            serde_json::to_string(&call).unwrap(),
            r#"{"call":3,"side":"buy","horizon_slots":160,"confidence":0.71}"#
        );
    }
}
