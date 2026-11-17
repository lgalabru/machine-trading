//! What the rungs read: the oracle's path and the market's takes.
//!
//! Both are public: the oracle every step hands the bot, and the takes any
//! reader of the chain sees. Where the session reports a tape (spot), the
//! takes come from it; on Phoenix's perpetuals they are read off the chain
//! as the cards' references read them: `getSignaturesForAddress` on the
//! market's order book, then `getTransaction`, the first signer and the side
//! and size of each order that can take (post-only orders rest and are left
//! out).

use std::collections::{BTreeMap, HashSet, VecDeque};

use mtx_kit::{
    Market, Session, rpc,
    venue::{
        PhoenixVenue,
        phoenix_rise::{
            ix::{PhoenixInstruction, order_packet::OrderPacket},
            types::exchange::ExchangeMarketConfig,
        },
    },
};
use serde_json::{Value, json};

/// The oracle's path, one point a step.
#[derive(Debug, Default)]
pub struct Trail {
    points: VecDeque<(u64, f64)>,
    /// Per-step log returns, for their spread.
    returns: VecDeque<f64>,
}

/// Steps of the path kept: enough for any look-back a rung uses.
const TRAIL_STEPS: usize = 4096;

impl Trail {
    pub fn push(&mut self, slot: u64, oracle: f64) {
        if !(oracle.is_finite() && oracle > 0.0) {
            return;
        }
        if let Some(&(_, last)) = self.points.back() {
            self.returns.push_back((oracle / last).ln());
            if self.returns.len() > TRAIL_STEPS {
                self.returns.pop_front();
            }
        }
        self.points.push_back((slot, oracle));
        if self.points.len() > TRAIL_STEPS {
            self.points.pop_front();
        }
    }

    /// The return over the last `look` slots, or None before there is one.
    pub fn ret(&self, look: u64) -> Option<f64> {
        let &(now, last) = self.points.back()?;
        let since = now.checked_sub(look)?;
        let &(_, then) = self.points.iter().rev().find(|(s, _)| *s <= since)?;
        Some(last / then - 1.0)
    }

    /// The direction of a move over `look` slots that is `z` standard
    /// deviations of the path's own per-step noise or more, once `min` steps
    /// have shown that noise; the threshold scales with the square root of
    /// the steps the move took.
    pub fn moved(&self, look: u64, z: f64, min: usize) -> Option<f64> {
        if self.returns.len() < min {
            return None;
        }
        let n = self.returns.len() as f64;
        let mean = self.returns.iter().sum::<f64>() / n;
        let var = self.returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let &(now, _) = self.points.back()?;
        let steps = self
            .points
            .iter()
            .rev()
            .take_while(|(s, _)| *s + look > now)
            .count()
            .max(1) as f64;
        let threshold = z * (var * steps).sqrt();
        let r = self.ret(look)?;
        (threshold > 0.0 && r.abs() >= threshold).then_some(r.signum())
    }
}

/// One take: a market order or a limit order that can cross.
#[derive(Debug, Clone, PartialEq)]
pub struct Take {
    pub slot: u64,
    pub wallet: String,
    /// Side times size, base units: positive bought.
    pub signed: f64,
}

/// Where the takes come from.
enum Source {
    /// The session's turn data.
    Turns,
    /// Phoenix's order book account, read off the chain.
    Chain {
        venue: Box<PhoenixVenue>,
        market: Box<ExchangeMarketConfig>,
        book: String,
    },
    /// Nothing to read.
    None,
}

/// Every take seen so far, and each wallet's tally.
pub struct Tape {
    source: Source,
    me: String,
    seen: HashSet<String>,
    /// Oldest first.
    pub takes: Vec<Take>,
    /// Per wallet: takes and volume, in base units. Ordered, so a tie breaks
    /// the same way in every run.
    pub wallets: BTreeMap<String, (u64, f64)>,
}

impl Default for Tape {
    fn default() -> Self {
        Self {
            source: Source::None,
            me: String::new(),
            seen: HashSet::new(),
            takes: Vec::new(),
            wallets: BTreeMap::new(),
        }
    }
}

/// The most signatures read in one step.
const SIGNATURES: u64 = 100;

impl Tape {
    pub fn of(session: &Session) -> Self {
        let source = if session.reports.tape {
            Source::Turns
        } else {
            match PhoenixVenue::from_json(&session.venue).and_then(|venue| {
                let market = venue.market(&session.symbol)?.clone();
                Ok((venue, market))
            }) {
                Ok((venue, market)) => Source::Chain {
                    book: market.market_pubkey.to_string(),
                    venue: Box::new(venue),
                    market: Box::new(market),
                },
                Err(_) => Source::None,
            }
        };
        Self {
            source,
            me: session.authority.clone(),
            ..Self::default()
        }
    }

    /// Read the takes since the last step; the new ones, oldest first.
    pub fn read(&mut self, market: &Market) -> &[Take] {
        let start = self.takes.len();
        let fresh: Vec<Take> = match &self.source {
            Source::Turns => market
                .tape
                .iter()
                .filter(|t| t.own.is_none())
                .filter_map(|t| {
                    let sign = match t.side {
                        mtx_core::Side::Buy => 1.0,
                        mtx_core::Side::Sell => -1.0,
                    };
                    Some(Take {
                        slot: t.slot,
                        wallet: t.taker.clone()?,
                        signed: sign * t.base,
                    })
                })
                .collect(),
            Source::Chain {
                venue,
                market,
                book,
            } => chain_takes(venue, market, book, &mut self.seen),
            Source::None => Vec::new(),
        };
        for take in fresh {
            if take.wallet == self.me || !take.signed.is_finite() || take.signed == 0.0 {
                continue;
            }
            let tally = self.wallets.entry(take.wallet.clone()).or_default();
            tally.0 += 1;
            tally.1 += take.signed.abs();
            self.takes.push(take);
        }
        &self.takes[start..]
    }

    /// The net signed size of the takes since `since` (a slot), of `wallet`
    /// only when given.
    pub fn net(&self, since: u64, wallet: Option<&str>) -> f64 {
        self.takes
            .iter()
            .rev()
            .take_while(|t| t.slot >= since)
            .filter(|t| wallet.is_none_or(|w| t.wallet == w))
            .map(|t| t.signed)
            .sum()
    }

    /// The median size of a take so far, once there are `min` of them.
    pub fn median_size(&self, min: usize) -> Option<f64> {
        if self.takes.len() < min {
            return None;
        }
        let mut sizes: Vec<f64> = self.takes.iter().map(|t| t.signed.abs()).collect();
        sizes.sort_by(f64::total_cmp);
        Some(sizes[sizes.len() / 2])
    }

    /// The wallet with the most takes (`by_volume`: the most volume), among
    /// wallets with at least `min` takes; the first in order on a tie.
    pub fn leader(&self, by_volume: bool, min: u64) -> Option<&str> {
        let mut best: Option<(&str, f64)> = None;
        for (wallet, &(count, volume)) in &self.wallets {
            if count < min {
                continue;
            }
            let score = if by_volume { volume } else { count as f64 };
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((wallet, score));
            }
        }
        best.map(|(w, _)| w)
    }
}

/// The takes on `book` not seen before, oldest first.
fn chain_takes(
    venue: &PhoenixVenue,
    market: &ExchangeMarketConfig,
    book: &str,
    seen: &mut HashSet<String>,
) -> Vec<Take> {
    let call = |method: &str, params: Value| -> Option<Value> {
        serde_json::from_str(&rpc(method, &params.to_string()).ok()?).ok()
    };
    let Some(signatures) = call(
        "getSignaturesForAddress",
        json!([book, { "limit": SIGNATURES, "commitment": "confirmed" }]),
    ) else {
        return Vec::new();
    };
    let mut fresh: Vec<(u64, String)> = signatures
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["err"].is_null())
        .filter_map(|s| Some((s["slot"].as_u64()?, s["signature"].as_str()?.to_string())))
        .filter(|(_, sig)| !seen.contains(sig))
        .collect();
    fresh.sort();
    let mut out = Vec::new();
    for (slot, sig) in fresh {
        seen.insert(sig.clone());
        let Some(tx) = call(
            "getTransaction",
            json!([sig, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]),
        ) else {
            continue;
        };
        out.extend(
            takes_in(&tx, venue, market)
                .into_iter()
                .map(|(wallet, signed)| Take {
                    slot,
                    wallet,
                    signed,
                }),
        );
    }
    out
}

/// The takes in one transaction (`getTransaction`, json): its first signer,
/// and side times size of each order that can take.
fn takes_in(tx: &Value, venue: &PhoenixVenue, market: &ExchangeMarketConfig) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    if !tx["meta"]["err"].is_null() {
        return out;
    }
    let message = &tx["transaction"]["message"];
    let Some(signer) = message["accountKeys"]
        .as_array()
        .and_then(|keys| keys.first())
        .and_then(Value::as_str)
    else {
        return out;
    };
    for ix in message["instructions"].as_array().into_iter().flatten() {
        let data = ix["data"]
            .as_str()
            .and_then(|d| bs58::decode(d).into_vec().ok())
            .unwrap_or_default();
        let Some(kind) = data
            .get(..8)
            .and_then(|tag| PhoenixInstruction::from_discriminant(tag.try_into().ok()?))
        else {
            continue;
        };
        if !matches!(
            kind,
            PhoenixInstruction::PlaceLimitOrder | PhoenixInstruction::PlaceMarketOrder
        ) {
            continue;
        }
        let Ok(packet) = <OrderPacket as borsh::BorshDeserialize>::deserialize(&mut &data[8..])
        else {
            continue;
        };
        let Ok(value) = serde_json::to_value(&packet) else {
            continue;
        };
        let Some((variant, fields)) = value["kind"].as_object().and_then(|o| o.iter().next())
        else {
            continue;
        };
        if variant.to_ascii_lowercase().contains("post") {
            continue;
        }
        let side = match fields["side"].as_str() {
            Some(s) if s.eq_ignore_ascii_case("bid") => 1.0,
            Some(_) => -1.0,
            None => continue,
        };
        let Some(lots) = fields["num_base_lots"].as_i64() else {
            continue;
        };
        let size = venue.lots_to_base_units(market, lots);
        if size > 0.0 {
            out.push((signer.to_string(), side * size));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(slot: u64, wallet: &str, signed: f64) -> Take {
        Take {
            slot,
            wallet: wallet.to_string(),
            signed,
        }
    }

    fn tape(takes: &[Take]) -> Tape {
        let mut t = Tape::default();
        for x in takes {
            let tally = t.wallets.entry(x.wallet.clone()).or_default();
            tally.0 += 1;
            tally.1 += x.signed.abs();
            t.takes.push(x.clone());
        }
        t
    }

    #[test]
    fn a_return_looks_back_to_the_last_point_at_or_before_its_start() {
        let mut t = Trail::default();
        assert_eq!(t.ret(10), None);
        for (s, p) in [(0, 100.0), (10, 101.0), (20, 102.0)] {
            t.push(s, p);
        }
        t.push(25, f64::NAN);
        assert!((t.ret(10).unwrap() - (102.0 / 101.0 - 1.0)).abs() < 1e-12);
        assert!((t.ret(15).unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(t.ret(30), None);
    }

    #[test]
    fn a_move_counts_only_past_the_paths_own_noise() {
        let mut t = Trail::default();
        for k in 0..100u64 {
            let wiggle = if k % 2 == 0 { 1.0 } else { 1.0005 };
            t.push(k * 2, 100.0 * wiggle);
        }
        assert_eq!(t.moved(20, 2.0, 30), None);
        t.push(200, 101.0);
        assert_eq!(t.moved(20, 2.0, 30), Some(1.0));
        assert_eq!(t.moved(20, 2.0, 500), None, "too little noise seen yet");
    }

    #[test]
    fn the_tape_tallies_wallets_and_breaks_ties_in_order() {
        let t = tape(&[
            take(1, "b", 2.0),
            take(2, "a", -1.0),
            take(3, "b", -1.0),
            take(4, "a", 5.0),
            take(9, "c", 0.5),
        ]);
        assert_eq!(t.leader(false, 2), Some("a"));
        assert_eq!(t.leader(true, 2), Some("a"));
        assert_eq!(t.leader(false, 3), None);
        assert_eq!(t.net(3, None), 4.5);
        assert_eq!(t.net(0, Some("b")), 1.0);
        assert_eq!(t.median_size(5), Some(1.0));
        assert_eq!(t.median_size(6), None);
    }
}
