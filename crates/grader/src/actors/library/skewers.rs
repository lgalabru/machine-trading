//! Skewers: rival makers that rest a two-sided quote around the market's
//! reference and lean it the way they expect the price to go.
//!
//! Each skewer is one wallet. After the blocks it looks at its view: `+1`
//! (up), `-1` (down) or `0`. On a look that finds its quote due (every
//! `react_slots`, drawn within `jitter` each time) or its view changed, it
//! cancels what it rests and quotes again around the reference (the
//! market's index, a perp's, else the scenario's level): a bid
//! `half_bps - skew_bps * view` under it and an ask `half_bps + skew_bps *
//! view` over it, each side's distance off by a normal draw of
//! `quote_noise_bps`, and never nearer the reference than a tick. Leaning
//! up raises both sides: the bid nears the reference, where flow fills it,
//! and the ask steps back. Between looks its quote stands where it was, so
//! a move it didn't expect leaves it stale, and the arbitrageur takes what
//! the move put through the new reference.
//!
//! Where its view comes from is its `view`:
//!
//! - **`informed`** sees the scenario's level `horizon_slots` ahead and
//!   leans its way while the move is over `edge_bps`, as the informed
//!   traders read it (`hit_rate`, `view_noise_bps`): wrong for a whole
//!   horizon at a time, now and then.
//! - **`decoy`** leans on random views of its own: one way or the other
//!   with equal odds, held for `hold_slots`, then flat for `gap_slots`
//!   (each drawn uniformly in its range, every time), so it is a maker that
//!   leans on a model that knows nothing. A scenario that sets the ranges
//!   from the informed skewer's own rhythm leaves its timing no label.
//! - **`none`** never leans: a plain two-sided maker.
//!
//! Sizes are drawn per quote, lognormally at `size_sigma` around
//! `size_mult` trade sizes, in whole lots. It stops adding to a side once
//! it holds `max_position_mult` trade sizes that way. Every quote is an
//! ordinary order from its wallet, so a reader of the book sees each
//! skewer's quotes, and of the chain, its position.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{
    JITTER, Rng, Scale, Wallet, Window, finite_at_least_zero, finite_positive, jittered,
    round_down, round_up, sized, traders::View, within,
};
use crate::actors::{Actor, ActorsReport, Moment};

fn five() -> f64 {
    5.0
}

fn ten() -> u64 {
    10
}

fn one() -> f64 {
    1.0
}

fn hundred() -> f64 {
    100.0
}

fn jitter() -> f64 {
    JITTER
}

fn most() -> f64 {
    0.85
}

/// A skewer's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Skewer {
    /// Where its leaning comes from.
    #[serde(default)]
    pub view: SkewView,
    /// Each side's distance from the reference with no view, in basis
    /// points, and how far a full view moves both sides, at most that.
    #[serde(default = "five")]
    pub half_bps: f64,
    #[serde(default)]
    pub skew_bps: f64,
    /// The sigma of a normal draw added to each side's distance at each
    /// quote, in basis points.
    #[serde(default)]
    pub quote_noise_bps: f64,
    /// Slots between its quotes, drawn within `jitter` of this each time.
    #[serde(default = "ten")]
    pub react_slots: u64,
    #[serde(default = "jitter")]
    pub jitter: f64,
    /// Each side's size, in the market's trade sizes, drawn lognormally at
    /// `size_sigma` (by default 0.3).
    #[serde(default = "one")]
    pub size_mult: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_sigma: Option<f64>,
    /// The most it holds either way, in trade sizes: past it, it quotes only
    /// the side that trades it back.
    #[serde(default = "hundred")]
    pub max_position_mult: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

/// What a skewer leans on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkewView {
    /// No view: it never leans.
    #[default]
    None,
    /// The level `horizon_slots` ahead, while it is over `edge_bps` off, as
    /// the informed traders see it.
    Informed {
        horizon_slots: u64,
        edge_bps: f64,
        #[serde(default = "most")]
        hit_rate: f64,
        #[serde(default)]
        view_noise_bps: f64,
    },
    /// Random views held for `hold_slots`, with `gap_slots` flat between,
    /// each drawn uniformly in its `[low, high]`.
    Decoy {
        hold_slots: [u64; 2],
        gap_slots: [u64; 2],
    },
}

impl Skewer {
    /// Its kind, as the report and the roster name it.
    pub fn kind(&self) -> &'static str {
        match self.view {
            SkewView::None => "skewer",
            SkewView::Informed { .. } => "skewer-informed",
            SkewView::Decoy { .. } => "skewer-decoy",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        finite_positive("half_bps", self.half_bps)?;
        finite_at_least_zero("skew_bps", self.skew_bps)?;
        if self.skew_bps > self.half_bps {
            return Err("skew_bps must be at most half_bps".into());
        }
        finite_at_least_zero("quote_noise_bps", self.quote_noise_bps)?;
        if self.react_slots == 0 {
            return Err("react_slots must be at least 1".into());
        }
        within("jitter", self.jitter, 0.0, 0.9)?;
        finite_positive("size_mult", self.size_mult)?;
        if let Some(sigma) = self.size_sigma {
            within("size_sigma", sigma, 0.0, 3.0)?;
        }
        finite_positive("max_position_mult", self.max_position_mult)?;
        match &self.view {
            SkewView::None => Ok(()),
            SkewView::Informed {
                horizon_slots,
                edge_bps,
                hit_rate,
                view_noise_bps,
            } => {
                if *horizon_slots == 0 {
                    return Err("view.horizon_slots must be at least 1".into());
                }
                finite_at_least_zero("view.edge_bps", *edge_bps)?;
                within("view.hit_rate", *hit_rate, 0.5, 1.0)?;
                finite_at_least_zero("view.view_noise_bps", *view_noise_bps)
            }
            SkewView::Decoy {
                hold_slots,
                gap_slots,
            } => {
                if hold_slots[0] == 0 || hold_slots[0] > hold_slots[1] {
                    return Err("view.hold_slots must be an ascending range from 1".into());
                }
                if gap_slots[0] > gap_slots[1] {
                    return Err("view.gap_slots must be an ascending range".into());
                }
                Ok(())
            }
        }
    }
}

/// A skewer's view as it runs.
enum Leaning {
    None,
    Informed {
        horizon: u64,
        view: View,
    },
    /// The view it holds and until when; at 0, flat until then.
    Decoy {
        hold: [u64; 2],
        gap: [u64; 2],
        lean: i8,
        until: u64,
    },
}

fn uniform_in(rng: &mut Rng, range: [u64; 2]) -> u64 {
    range[0] + rng.below((range[1] - range[0] + 1) as usize) as u64
}

pub(super) struct SkewerActor {
    spec: Skewer,
    scale: Scale,
    wallet: Wallet,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    rng: Rng,
    leaning: Leaning,
    /// The view its standing quote leans on, and when it quotes next.
    quoted: Option<i8>,
    next: u64,
}

impl SkewerActor {
    pub fn new(
        spec: Skewer,
        scale: Scale,
        wallet: Wallet,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
        mut rng: Rng,
    ) -> Self {
        let leaning = match spec.view {
            SkewView::None => Leaning::None,
            SkewView::Informed {
                horizon_slots,
                edge_bps,
                hit_rate,
                view_noise_bps,
            } => Leaning::Informed {
                horizon: horizon_slots,
                view: View::new(horizon_slots, edge_bps, hit_rate, view_noise_bps),
            },
            SkewView::Decoy {
                hold_slots,
                gap_slots,
            } => {
                // It starts part way through a flat spell, so the session's
                // first slot is no phase of its rhythm.
                let first = uniform_in(&mut rng, gap_slots);
                Leaning::Decoy {
                    hold: hold_slots,
                    gap: gap_slots,
                    lean: 0,
                    until: rng.below(first as usize + 1) as u64,
                }
            }
        };
        Self {
            spec,
            scale,
            wallet,
            path,
            rng,
            leaning,
            quoted: None,
            next: 0,
        }
    }

    /// Its view at `now`: `1` up, `-1` down, `0` none.
    fn lean(&mut self, now: &Moment) -> i8 {
        match &mut self.leaning {
            Leaning::None => 0,
            Leaning::Informed { horizon, view } => {
                let ahead = (self.path)(now.at + *horizon).book_usd;
                match view.side(
                    &mut self.rng,
                    now.at,
                    (ahead / now.book_usd - 1.0) * 10_000.0,
                ) {
                    Some(Side::Bid) => 1,
                    Some(Side::Ask) => -1,
                    None => 0,
                }
            }
            Leaning::Decoy {
                hold,
                gap,
                lean,
                until,
            } => {
                while now.at >= *until {
                    if *lean == 0 {
                        *lean = if self.rng.chance(0.5) { 1 } else { -1 };
                        *until += uniform_in(&mut self.rng, *hold);
                    } else {
                        *lean = 0;
                        *until += uniform_in(&mut self.rng, *gap).max(1);
                    }
                }
                *lean
            }
        }
    }

    /// One side's size, in whole lots.
    fn size(&mut self, lot: f64) -> f64 {
        let size = sized(
            &self.scale,
            &mut self.rng,
            self.spec.size_mult,
            self.spec.size_sigma,
        );
        if lot > 0.0 {
            (size / lot).floor().max(1.0) * lot
        } else {
            size
        }
    }

    async fn quote(&mut self, now: &Moment, lean: i8) {
        let seat = self.wallet.seat();
        let tick = seat.tick();
        let lot = seat.lot();
        let reference = match seat.index().await {
            Ok(Some(index)) => index,
            _ => now.level_usd,
        };
        let held = seat.position().await.ok();
        if self.quoted.is_some() && self.wallet.cancel_all(now.at).await {
            self.quoted = None;
        }
        let skew = self.spec.skew_bps * f64::from(lean);
        let noise = self.spec.quote_noise_bps;
        let mut away = |base: f64| {
            let off = if noise > 0.0 {
                noise * self.rng.normal()
            } else {
                0.0
            };
            (base + off).max(0.0) / 10_000.0
        };
        let bid_away = away(self.spec.half_bps - skew);
        let ask_away = away(self.spec.half_bps + skew);
        let (mut bid, mut ask) = (reference * (1.0 - bid_away), reference * (1.0 + ask_away));
        if tick > 0.0 {
            bid = round_down(bid, tick).min(round_down(reference - tick, tick));
            ask = round_up(ask, tick).max(round_up(reference + tick, tick));
        }
        let (bid_size, ask_size) = (self.size(lot), self.size(lot));
        let limit = self.scale.trade_size * self.spec.max_position_mult;
        let held = held.unwrap_or(0.0);
        let mut rested = false;
        if held < limit && bid > 0.0 {
            rested |= self.wallet.rest(now.at, Side::Bid, bid, bid_size).await;
        }
        if held > -limit {
            rested |= self.wallet.rest(now.at, Side::Ask, ask, ask_size).await;
        }
        if rested {
            self.quoted = Some(lean);
            self.wallet.count("quotes", 1);
            if lean != 0 {
                self.wallet.count("leaning_quotes", 1);
            }
        }
        self.next = now.at + jittered(&mut self.rng, self.spec.react_slots, self.spec.jitter, 1);
    }
}

#[async_trait]
impl Actor for SkewerActor {
    async fn after_blocks(&mut self, now: &Moment) {
        if !Window::holds(&self.spec.active, now.at) {
            if self.quoted.is_some() && self.wallet.cancel_all(now.at).await {
                self.quoted = None;
            }
            return;
        }
        let lean = self.lean(now);
        if now.at < self.next && self.quoted == Some(lean) {
            return;
        }
        self.quote(now, lean).await;
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::tests::{Book, at, scale, wallet};

    fn spec(view: SkewView) -> Skewer {
        Skewer {
            view,
            half_bps: 10.0,
            skew_bps: 8.0,
            quote_noise_bps: 0.0,
            react_slots: 10,
            jitter: 0.0,
            size_mult: 1.0,
            size_sigma: Some(0.0),
            max_position_mult: 100.0,
            active: None,
        }
    }

    /// The bid and ask each quote rested, in order.
    fn quotes(log: &[String]) -> Vec<(f64, f64)> {
        let price = |l: &String| l.rsplit(' ').next().unwrap().parse::<f64>().unwrap();
        let bids: Vec<f64> = log
            .iter()
            .filter(|l| l.contains("rest Bid"))
            .map(price)
            .collect();
        let asks: Vec<f64> = log
            .iter()
            .filter(|l| l.contains("rest Ask"))
            .map(price)
            .collect();
        bids.into_iter().zip(asks).collect()
    }

    #[tokio::test]
    async fn an_informed_skewer_leans_its_quote_the_way_the_path_goes() {
        let (book, log) = Book::new("s", 99.0, 101.0);
        let (wallet, ledger) = wallet(book);
        // Up 1% from slot 100.
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> =
            Arc::new(|s| at(s, if s >= 100 { 101.0 } else { 100.0 }));
        let view = SkewView::Informed {
            horizon_slots: 20,
            edge_bps: 50.0,
            hit_rate: 1.0,
            view_noise_bps: 0.0,
        };
        let mut skewer = SkewerActor::new(spec(view), scale(), wallet, path, Rng::new([3; 32]));
        // Flat: 10 bp either side.
        skewer.after_blocks(&at(60, 100.0)).await;
        // Not due, and its view hasn't changed.
        skewer.after_blocks(&at(62, 100.0)).await;
        // It sees the rise and quotes again at once, leaning up.
        skewer.after_blocks(&at(82, 100.0)).await;
        let log = log.lock().unwrap().clone();
        assert_eq!(quotes(&log), [(99.9, 100.1), (99.98, 100.18)], "{log:?}");
        assert_eq!(log.iter().filter(|l| l.ends_with("cancel")).count(), 1);
        let counts = &ledger.report().actors[0].counts;
        assert_eq!((counts["quotes"], counts["leaning_quotes"]), (2, 1));
    }

    #[tokio::test]
    async fn a_full_lean_never_quotes_through_the_reference() {
        let (book, log) = Book::new("s", 99.0, 101.0);
        let (wallet, _) = wallet(book);
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> =
            Arc::new(|s| at(s, if s >= 100 { 90.0 } else { 100.0 }));
        let view = SkewView::Informed {
            horizon_slots: 20,
            edge_bps: 50.0,
            hit_rate: 1.0,
            view_noise_bps: 0.0,
        };
        let mut full = spec(view);
        full.skew_bps = full.half_bps;
        full.quote_noise_bps = 3.0;
        let mut skewer = SkewerActor::new(full, scale(), wallet, path, Rng::new([8; 32]));
        for s in (82..96).step_by(2) {
            skewer.after_blocks(&at(s, 100.0)).await;
            skewer.next = 0;
        }
        let quoted = quotes(&log.lock().unwrap());
        assert!(quoted.len() >= 5);
        assert!(
            quoted.iter().all(|(b, a)| *b < 100.0 && *a > 100.0),
            "{quoted:?}"
        );
    }

    #[tokio::test]
    async fn a_decoy_holds_random_views_with_flat_spells_between() {
        let (book, log) = Book::new("d", 99.0, 101.0);
        let (wallet, _) = wallet(book);
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> = Arc::new(|s| at(s, 100.0));
        let view = SkewView::Decoy {
            hold_slots: [40, 60],
            gap_slots: [100, 140],
        };
        let mut skewer = SkewerActor::new(spec(view), scale(), wallet, path, Rng::new([5; 32]));
        let mut leans = Vec::new();
        for s in (2..20_000).step_by(2) {
            skewer.after_blocks(&at(s, 100.0)).await;
            if let Some((bid, ask)) = quotes(&std::mem::take(&mut *log.lock().unwrap())).pop() {
                let mid = (bid + ask) / 2.0;
                leans.push((s, ((mid - 100.0) * 100.0).round() as i64));
            }
        }
        let up = leans.iter().filter(|(_, l)| *l > 0).count();
        let down = leans.iter().filter(|(_, l)| *l < 0).count();
        let flat = leans.iter().filter(|(_, l)| *l == 0).count();
        assert!(up > 0 && down > 0 && flat > up + down, "{up} {down} {flat}");
        // About 50 of every 170 slots lean, and about half of those up.
        let share = (up + down) as f64 / leans.len() as f64;
        assert!((0.2..0.4).contains(&share), "{share}");
        assert!((up as f64 / (up + down) as f64 - 0.5).abs() < 0.15);
    }

    #[tokio::test]
    async fn past_its_limit_it_quotes_only_the_side_that_trades_it_back() {
        let (book, log) = Book::new("s", 99.0, 101.0);
        let position = book.position.clone();
        let (wallet, _) = wallet(book);
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> = Arc::new(|s| at(s, 100.0));
        let mut limited = spec(SkewView::None);
        limited.max_position_mult = 2.0;
        *position.lock().unwrap() = 3.0;
        let mut skewer = SkewerActor::new(limited, scale(), wallet, path, Rng::new([2; 32]));
        skewer.after_blocks(&at(2, 100.0)).await;
        let log = log.lock().unwrap().clone();
        assert_eq!(log, ["s rest Ask 1.000 at 100.10"]);
    }

    #[test]
    fn settings_are_checked() {
        let ok: Skewer = serde_json::from_value(serde_json::json!({
            "view": { "kind": "decoy", "hold_slots": [150, 400], "gap_slots": [300, 900] },
            "half_bps": 6.0,
            "skew_bps": 4.0,
        }))
        .unwrap();
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(ok.kind(), "skewer-decoy");
        let mut bad = ok.clone();
        bad.skew_bps = 7.0;
        assert!(bad.validate().unwrap_err().contains("skew_bps"));
        bad = ok.clone();
        bad.view = SkewView::Decoy {
            hold_slots: [0, 10],
            gap_slots: [1, 2],
        };
        assert!(bad.validate().unwrap_err().contains("hold_slots"));
        let unknown: Result<Skewer, _> = serde_json::from_value(serde_json::json!({
            "view": { "kind": "informed", "horizon_slots": 10, "edge_bps": 5.0, "nope": 1 },
        }));
        assert!(unknown.is_err());
        let plain: Skewer = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!((plain.kind(), plain.validate()), ("skewer", Ok(())));
    }
}
