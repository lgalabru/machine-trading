//! Execution orders, close-outs and the participation charge: what a
//! scenario can ask of the bot beyond making money, and what it costs to
//! break.
//!
//! An [`ExecutionOrder`] is a quantity the bot has to trade by the end of
//! the session, one way: sell it (a treasury working a position off), or buy
//! it (a treasury building one), from a spot wallet or in a perp market. Whatever it hasn't traded by then, a broker
//! trades for it at the session's closing price, worse by the order's
//! haircut. The session's PnL is the bot's against doing nothing, so a bot
//! that never trades scores 0 even when the price moves, and each unit it
//! trades is worth the haircut it saved:
//! `PnL = value change - start base × (close - start price) + haircut × close × done`.
//!
//! Trading the other way is a breach, and so is, for a `reduce_only` order,
//! any fill that grows the position. Neither is in the mandate, which has no
//! "sell by the end": the order belongs to the scenario's scoring.
//!
//! A [`CloseOut`] has a broker take whatever position is left in a perp
//! market at the close, with no quantity to reach: the residual is valued at
//! the broker's price instead of the close, and the difference comes off the
//! PnL.
//!
//! [`Participation`] prices taking too much of a venue's volume. It is a
//! charge in the PnL, not a rule: a bot that trades big pays for it.

use mtx_core::{Side, mandate::InstrumentRef};
use serde::{Deserialize, Serialize};

/// A quantity to trade by the end of the session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOrder {
    /// The market the order is in, for a perp session (the scenario's
    /// market when absent). A spot order is in the wallet's base asset,
    /// wherever it trades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<InstrumentRef>,
    /// `sell` to get rid of the quantity, `buy` to acquire it.
    pub side: Side,
    /// Base units the bot is to trade by the end.
    pub quantity: f64,
    /// What the broker's price for the remainder is worse than the close by,
    /// in basis points: lower when it sells, higher when it buys.
    pub haircut_bps: f64,
    /// Whether any fill that grows the position is a breach. On a spot
    /// wallet, which can't go short, a sell order already breaks on any buy,
    /// so this adds nothing there; it is for markets where the position can
    /// flip, such as a perp, where a reduce-only buy closes a short.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reduce_only: bool,
}

impl ExecutionOrder {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(self.quantity.is_finite() && self.quantity > 0.0) {
            return Err(format!(
                "order: quantity {} must be positive",
                self.quantity
            ));
        }
        if !(self.haircut_bps.is_finite() && self.haircut_bps >= 0.0) {
            return Err(format!(
                "order: haircut_bps {} must be at least 0",
                self.haircut_bps
            ));
        }
        Ok(())
    }

    /// The broker's price for what is left, at the close.
    pub fn broker_usd(&self, close_usd: f64) -> f64 {
        let haircut = self.haircut_bps / 10_000.0;
        match self.side {
            Side::Sell => close_usd * (1.0 - haircut),
            Side::Buy => close_usd * (1.0 + haircut),
        }
    }

    /// How the session went against the order: `start_base` and `end_base`
    /// are the base units the wallet held, `start_usd` and `close_usd` the
    /// price they were valued at, `value_change_usd` the account's change in
    /// value, and `fills` the bot's own fills (side, base units), oldest
    /// first.
    pub fn report(
        &self,
        start_base: f64,
        end_base: f64,
        start_usd: f64,
        close_usd: f64,
        value_change_usd: f64,
        fills: &[(Side, f64)],
    ) -> OrderReport {
        let moved = match self.side {
            Side::Sell => start_base - end_base,
            Side::Buy => end_base - start_base,
        };
        let mut against = 0.0;
        let mut grown = 0.0;
        let mut held = start_base;
        for &(side, base) in fills {
            if side != self.side {
                against += base;
            }
            let after = held + if side == Side::Buy { base } else { -base };
            if after.abs() > held.abs() {
                grown += after.abs() - held.abs();
            }
            held = after;
        }
        OrderReport {
            instrument: self.instrument.clone(),
            side: self.side,
            quantity: self.quantity,
            done: moved.clamp(0.0, self.quantity),
            start_usd,
            close_usd,
            broker_usd: self.broker_usd(close_usd),
            value_change_usd,
            drift_usd: start_base * (close_usd - start_usd),
            against,
            grown: if self.reduce_only { grown } else { 0.0 },
        }
    }
}

/// An order session's account of the order. The session's PnL is the bot's
/// against doing nothing, which the broker would have done at its price.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderReport {
    /// The order's market, for a perp session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<InstrumentRef>,
    pub side: Side,
    pub quantity: f64,
    /// Base units traded toward the order by the end, at most the quantity.
    pub done: f64,
    /// The price the account was valued at at the start and the close, in
    /// USD, and the broker's price for the remainder.
    pub start_usd: f64,
    pub close_usd: f64,
    pub broker_usd: f64,
    /// What the account's value changed by over the session, valued at the
    /// close; the PnL takes off the drift and adds the haircut on each unit
    /// done.
    pub value_change_usd: f64,
    /// What the starting holding gained from the price moving, which doing
    /// nothing would have kept.
    pub drift_usd: f64,
    /// Base units the bot traded the other way, over the session.
    pub against: f64,
    /// Base units by which its fills grew the position (reduce-only orders).
    pub grown: f64,
}

impl OrderReport {
    /// The PnL against the broker: what the bot's trades earned beyond the
    /// price drift on what it started with, and each unit done is worth
    /// what the broker would have taken off it.
    pub fn pnl_usd(&self) -> f64 {
        self.value_change_usd - self.drift_usd
            + self.done * (self.broker_usd - self.close_usd).abs()
    }

    /// Why the session breaks the order's rules, if it does.
    pub fn breach(&self) -> Option<String> {
        let round = |x: f64| (x * 1000.0).round() / 1000.0;
        let unit = self
            .instrument
            .as_ref()
            .map_or("SOL", |i| i.symbol.as_str());
        if self.against > 1e-6 {
            let (did, only) = match self.side {
                Side::Sell => ("bought", "sells"),
                Side::Buy => ("sold", "buys"),
            };
            Some(format!(
                "{did} {} {unit}: an order to {} only {only}",
                round(self.against),
                match self.side {
                    Side::Sell => "sell",
                    Side::Buy => "buy",
                }
            ))
        } else if self.grown > 1e-6 {
            Some(format!(
                "grew the position by {} {unit}: the order is reduce-only",
                round(self.grown)
            ))
        } else {
            None
        }
    }
}

/// A broker taking a perp market's residual position at the close.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CloseOut {
    /// What the broker's price is worse than the close by, in basis points:
    /// lower for a long it sells, higher for a short it buys back.
    pub haircut_bps: f64,
}

impl CloseOut {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(self.haircut_bps.is_finite() && self.haircut_bps >= 0.0) {
            return Err(format!(
                "close_out: haircut_bps {} must be at least 0",
                self.haircut_bps
            ));
        }
        Ok(())
    }

    /// The broker's account of `residual` base units of `symbol` left at
    /// the close, `close_usd`.
    pub fn report(&self, symbol: &str, residual: f64, close_usd: f64) -> CloseOutReport {
        let haircut = self.haircut_bps / 10_000.0;
        let broker_usd = if residual < 0.0 {
            close_usd * (1.0 + haircut)
        } else {
            close_usd * (1.0 - haircut)
        };
        CloseOutReport {
            symbol: symbol.to_string(),
            residual,
            close_usd,
            broker_usd,
            charge_usd: residual.abs() * (broker_usd - close_usd).abs(),
        }
    }
}

/// What a close-out cost: the residual position, in base units, the close
/// and the broker's price for it, and the difference, which the PnL is net
/// of.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloseOutReport {
    pub symbol: String,
    pub residual: f64,
    pub close_usd: f64,
    pub broker_usd: f64,
    pub charge_usd: f64,
}

/// A limit on the bot's share of a venue's traded volume, priced: base units
/// over it cost `price_bps` of the price they traded at.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Participation {
    /// The most of a venue's traded base volume, in any window, that is the
    /// bot's before the charge starts: a share in (0, 1].
    pub max_share: f64,
    /// The window's length, in slots: it ends at each of the bot's fills.
    pub window_slots: u64,
    /// The charge on each base unit over the share, in basis points of the
    /// price it traded at.
    pub price_bps: f64,
}

impl Participation {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(self.max_share.is_finite() && self.max_share > 0.0 && self.max_share <= 1.0) {
            return Err(format!(
                "participation: max_share {} must be in (0, 1]",
                self.max_share
            ));
        }
        if self.window_slots == 0 {
            return Err("participation: the window has no slots".into());
        }
        if !(self.price_bps.is_finite() && self.price_bps >= 0.0) {
            return Err(format!(
                "participation: price_bps {} must be at least 0",
                self.price_bps
            ));
        }
        Ok(())
    }

    /// The base units over the share in each trade, parallel to `trades`
    /// (zero for others' trades). Venues count apart. A window is the
    /// `window_slots` slots up to and including a trade's slot, and holds
    /// every trade of the venue in it. At each of the bot's trades, the
    /// bot's volume in the window less `max_share` of the venue's is the
    /// excess; the trade is charged what is new in it, that is, the excess
    /// less what the bot's earlier trades in the window were already charged,
    /// up to the trade's size, so no unit is charged twice in a window.
    pub fn excess(&self, trades: &[Traded]) -> Vec<f64> {
        let mut out = vec![0.0; trades.len()];
        let mut order: Vec<usize> = (0..trades.len()).collect();
        order.sort_by_key(|&i| (trades[i].slot, i));
        for (n, &i) in order.iter().enumerate() {
            let t = &trades[i];
            if !t.own {
                continue;
            }
            let from = (t.slot + 1).saturating_sub(self.window_slots);
            let in_window = |u: &Traded| u.venue == t.venue && (from..=t.slot).contains(&u.slot);
            let total: f64 = trades.iter().filter(|u| in_window(u)).map(|u| u.base).sum();
            let mut bot = 0.0;
            let mut charged = 0.0;
            for &j in &order[..=n] {
                if trades[j].own && in_window(&trades[j]) {
                    bot += trades[j].base;
                    charged += out[j];
                }
            }
            let over = (bot - self.max_share * total).max(0.0);
            out[i] = (over - charged).clamp(0.0, t.base);
        }
        out
    }
}

/// A trade on a venue, as the participation charge counts it.
#[derive(Debug, Clone, PartialEq)]
pub struct Traded {
    pub venue: String,
    /// The price it traded at, quote units per base unit.
    pub price: f64,
    /// Slots into the session.
    pub slot: u64,
    pub base: f64,
    /// Whether the bot was one side of it.
    pub own: bool,
}

/// What the participation charge came to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParticipationReport {
    /// Base units charged, in all, and the charge in USD, which the PnL
    /// is net of.
    pub excess_base: f64,
    pub charge_usd: f64,
    /// Each venue's traded base volume over the session, and the bot's part
    /// of it.
    pub venues: Vec<VenueShare>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VenueShare {
    pub venue: String,
    pub traded_base: f64,
    pub own_base: f64,
    pub excess_base: f64,
}

impl Participation {
    /// The charge for a session's trades: each unit over the share costs
    /// `price_bps` of the price of the trade it was in.
    pub fn report(&self, trades: &[Traded], venues: &[String]) -> ParticipationReport {
        let excess = self.excess(trades);
        let shares: Vec<VenueShare> = venues
            .iter()
            .map(|venue| VenueShare {
                venue: venue.clone(),
                traded_base: trades
                    .iter()
                    .filter(|t| &t.venue == venue)
                    .map(|t| t.base)
                    .sum(),
                own_base: trades
                    .iter()
                    .filter(|t| &t.venue == venue && t.own)
                    .map(|t| t.base)
                    .sum(),
                excess_base: trades
                    .iter()
                    .zip(&excess)
                    .filter(|(t, _)| &t.venue == venue)
                    .map(|(_, e)| *e)
                    .sum(),
            })
            .collect();
        let excess_base: f64 = excess.iter().sum();
        ParticipationReport {
            excess_base,
            charge_usd: trades
                .iter()
                .zip(&excess)
                .map(|(t, e)| e * t.price * self.price_bps / 10_000.0)
                .sum(),
            venues: shares,
        }
    }
}

/// Two-sided scoring for a bot that starts with inventory and trades both
/// ways (an arbitrageur): its PnL is the account's value change less what
/// the starting holding gained from the price moving, so doing nothing
/// scores 0 whatever the price does, and nothing about the sides is a rule.
/// Two optional charges are priced into it, not enforced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetScoring {
    /// A charge on ending away from where it started: this many basis
    /// points of the value, at the close, of the change in base held.
    #[serde(default)]
    pub inventory_bps: f64,
    /// A charge on the base bought net on one venue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue_position: Option<VenuePosition>,
}

/// What a venue's net purchases cost beyond a limit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VenuePosition {
    /// Base units a venue's net purchases are free up to.
    pub limit: f64,
    /// The charge on each unit over it, in basis points of the closing price.
    pub price_bps: f64,
}

impl NetScoring {
    pub fn validate(&self) -> std::result::Result<(), String> {
        let ok = |x: f64| x.is_finite() && x >= 0.0;
        if !ok(self.inventory_bps) {
            return Err(format!(
                "net: inventory_bps {} must be at least 0",
                self.inventory_bps
            ));
        }
        if let Some(v) = &self.venue_position
            && !(ok(v.limit) && ok(v.price_bps))
        {
            return Err("net: venue_position needs a limit and price_bps of at least 0".into());
        }
        Ok(())
    }

    /// Score a session, with the (start, close) prices; `fills` are the bot's
    /// own (venue, side, base units).
    pub fn report(
        &self,
        start_base: f64,
        end_base: f64,
        (start_usd, close_usd): (f64, f64),
        value_change_usd: f64,
        fills: &[(String, Side, f64)],
        venues: &[String],
    ) -> NetReport {
        let drift_usd = start_base * (close_usd - start_usd);
        let inventory_charge_usd =
            (end_base - start_base).abs() * close_usd * self.inventory_bps / 10_000.0;
        let mut venue_net = Vec::new();
        let mut venue_charge_usd = 0.0;
        for venue in venues {
            let net: f64 = fills
                .iter()
                .filter(|(v, _, _)| v == venue)
                .map(|(_, side, base)| if *side == Side::Buy { *base } else { -*base })
                .sum();
            if let Some(v) = &self.venue_position {
                venue_charge_usd += (net - v.limit).max(0.0) * close_usd * v.price_bps / 10_000.0;
            }
            venue_net.push((venue.clone(), net));
        }
        NetReport {
            value_change_usd,
            drift_usd,
            inventory_charge_usd,
            venue_charge_usd,
            venue_net,
        }
    }
}

/// A net-scored session's account of its PnL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetReport {
    pub value_change_usd: f64,
    /// What the starting holding gained from the price moving.
    pub drift_usd: f64,
    pub inventory_charge_usd: f64,
    pub venue_charge_usd: f64,
    /// Base units bought net on each venue.
    pub venue_net: Vec<(String, f64)>,
}

impl NetReport {
    pub fn pnl_usd(&self) -> f64 {
        self.value_change_usd - self.drift_usd - self.inventory_charge_usd - self.venue_charge_usd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: Side) -> ExecutionOrder {
        ExecutionOrder {
            instrument: None,
            side,
            quantity: 10.0,
            haircut_bps: 100.0,
            reduce_only: false,
        }
    }

    fn trade(venue: &str, slot: u64, base: f64, own: bool) -> Traded {
        Traded {
            venue: venue.into(),
            price: 100.0,
            slot,
            base,
            own,
        }
    }

    #[test]
    fn doing_nothing_scores_zero_and_each_unit_done_earns_the_haircut() {
        let sell = order(Side::Sell);
        let nothing = sell.report(10.0, 10.0, 100.0, 100.0, 0.0, &[]);
        assert_eq!(nothing.done, 0.0);
        assert_eq!(nothing.pnl_usd(), 0.0);
        assert_eq!(nothing.breach(), None);
        // Sold 4 at 100.5: 402 in, 400 out at the close, 1% off the close
        // the broker takes: 4 units save 4.
        let sold = sell.report(
            10.0,
            6.0,
            100.0,
            100.0,
            4.0 * 100.5 - 400.0,
            &[(Side::Sell, 4.0)],
        );
        assert_eq!(sold.done, 4.0);
        assert!((sold.pnl_usd() - (2.0 + 4.0)).abs() < 1e-9);
        assert_eq!(sold.broker_usd, 99.0);
        // Selling past the quantity counts the quantity.
        assert_eq!(sell.report(20.0, 0.0, 100.0, 100.0, 0.0, &[]).done, 10.0);
    }

    #[test]
    fn doing_nothing_scores_zero_when_the_price_moves() {
        // 10 held at 100 and valued at 90 at the close: the account lost 100,
        // which doing nothing would have lost too.
        let mut reduce = order(Side::Sell);
        reduce.reduce_only = true;
        for o in [order(Side::Sell), reduce] {
            let nop = o.report(10.0, 10.0, 100.0, 90.0, -100.0, &[]);
            assert_eq!(nop.drift_usd, -100.0);
            assert_eq!(nop.pnl_usd(), 0.0);
        }
        // A buyer starting with 2 units and a close above its start.
        let nop = order(Side::Buy).report(2.0, 2.0, 100.0, 110.0, 20.0, &[]);
        assert_eq!(nop.pnl_usd(), 0.0);
        // Sold 4 at 95 with the close at 90: the broker would have paid 89.1
        // for them, so each unit is 5.9 better; 4 units, 23.6.
        let value_change = 6.0 * 90.0 + 4.0 * 95.0 - 10.0 * 100.0;
        let sold =
            order(Side::Sell).report(10.0, 6.0, 100.0, 90.0, value_change, &[(Side::Sell, 4.0)]);
        assert!((sold.pnl_usd() - 4.0 * (95.0 - 89.1)).abs() < 1e-9);
        assert!(sold.pnl_usd() > 0.0);
        // Bought 4 at 105 against a close of 110: the broker charges 111.1.
        let value_change = 6.0 * 110.0 + 4.0 * 110.0 - 6.0 * 100.0 - 4.0 * 105.0 + 0.0;
        let bought =
            order(Side::Buy).report(6.0, 10.0, 100.0, 110.0, value_change, &[(Side::Buy, 4.0)]);
        assert!((bought.pnl_usd() - 4.0 * (111.1 - 105.0)).abs() < 1e-9);
    }

    #[test]
    fn the_buy_order_mirrors_the_sale() {
        let buy = order(Side::Buy);
        assert_eq!(buy.broker_usd(100.0), 101.0);
        // Bought 4 at 99.5: 398 out, worth 400 at the close; the broker
        // would have charged 404 for them.
        let report = buy.report(0.0, 4.0, 100.0, 100.0, 400.0 - 398.0, &[(Side::Buy, 4.0)]);
        assert_eq!(report.done, 4.0);
        assert!((report.pnl_usd() - 6.0).abs() < 1e-9);
        assert_eq!(buy.report(0.0, 0.0, 100.0, 100.0, 0.0, &[]).pnl_usd(), 0.0);
        // Selling what it holds is the wrong way for a buy.
        let sold = buy.report(5.0, 4.0, 100.0, 100.0, 0.0, &[(Side::Sell, 1.0)]);
        assert!(sold.breach().unwrap().contains("only buys"));
    }

    #[test]
    fn trading_the_other_way_is_a_breach_even_if_it_nets_out() {
        let fills = [(Side::Sell, 3.0), (Side::Buy, 0.5), (Side::Sell, 3.0)];
        let report = order(Side::Sell).report(10.0, 3.5, 100.0, 100.0, 0.0, &fills);
        assert_eq!(report.against, 0.5);
        assert!(report.breach().unwrap().contains("only sells"));
    }

    #[test]
    fn reduce_only_breaches_a_fill_that_grows_the_position() {
        let mut sell = order(Side::Sell);
        sell.reduce_only = true;
        // Long 2 and selling 5 flips it short 3: the position grew past
        // flat by 1.
        let flipped = sell.report(2.0, -3.0, 100.0, 100.0, 0.0, &[(Side::Sell, 5.0)]);
        assert_eq!(flipped.grown, 3.0 - 2.0);
        assert!(flipped.breach().unwrap().contains("reduce-only"));
        let reduced = sell.report(5.0, 1.0, 100.0, 100.0, 0.0, &[(Side::Sell, 4.0)]);
        assert_eq!(reduced.breach(), None);
        // Not asked for: not reported.
        let plain = order(Side::Sell).report(2.0, -3.0, 100.0, 100.0, 0.0, &[(Side::Sell, 5.0)]);
        assert_eq!(plain.grown, 0.0);
        // On a perp, a reduce-only buy closes a short; buying past flat
        // grows the long it opens.
        let mut buy = order(Side::Buy);
        buy.reduce_only = true;
        buy.instrument = Some("phoenix:perp:NVDA".parse().unwrap());
        assert_eq!(buy.validate(), Ok(()));
        let covered = buy.report(-2.0, 3.0, 100.0, 100.0, 0.0, &[(Side::Buy, 5.0)]);
        assert_eq!(covered.grown, 1.0);
        assert_eq!(
            covered.breach().unwrap(),
            "grew the position by 1 NVDA: the order is reduce-only"
        );
    }

    #[test]
    fn a_close_out_charges_the_haircut_on_the_residual_either_way() {
        let close_out = CloseOut { haircut_bps: 50.0 };
        let long = close_out.report("SPY", 10.0, 200.0);
        assert!((long.broker_usd - 199.0).abs() < 1e-9);
        assert!((long.charge_usd - 10.0).abs() < 1e-9);
        let short = close_out.report("SPY", -4.0, 200.0);
        assert!((short.broker_usd - 201.0).abs() < 1e-9);
        assert!((short.charge_usd - 4.0).abs() < 1e-9);
        assert_eq!(close_out.report("SPY", 0.0, 200.0).charge_usd, 0.0);
        assert!(CloseOut { haircut_bps: -1.0 }.validate().is_err());
    }

    #[test]
    fn orders_and_participation_validate() {
        assert_eq!(order(Side::Sell).validate(), Ok(()));
        let mut bad = order(Side::Sell);
        bad.quantity = 0.0;
        assert!(bad.validate().is_err());
        bad = order(Side::Sell);
        bad.haircut_bps = -1.0;
        assert!(bad.validate().is_err());
        let ok = Participation {
            max_share: 0.1,
            window_slots: 10,
            price_bps: 5.0,
        };
        assert_eq!(ok.validate(), Ok(()));
        for (max_share, window_slots, price_bps) in [
            (0.0, 10, 5.0),
            (1.5, 10, 5.0),
            (0.1, 0, 5.0),
            (0.1, 10, -1.0),
        ] {
            let bad = Participation {
                max_share,
                window_slots,
                price_bps,
            };
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn participation_charges_what_is_over_the_share_at_its_edge() {
        let p = Participation {
            max_share: 0.25,
            window_slots: 10,
            price_bps: 10.0,
        };
        // 1 of 4 is exactly the share: nothing over.
        let at_edge = [
            trade("phoenix", 5, 3.0, false),
            trade("phoenix", 5, 1.0, true),
        ];
        assert_eq!(p.excess(&at_edge), vec![0.0, 0.0]);
        // 2 of 5 is 0.75 over a share of 1.25.
        let over = [
            trade("phoenix", 5, 3.0, false),
            trade("phoenix", 6, 2.0, true),
        ];
        assert_eq!(p.excess(&over), vec![0.0, 0.75]);
        // Nobody else trading: the bot is all of it, and 75% is over.
        let alone = [trade("phoenix", 5, 4.0, true)];
        assert_eq!(p.excess(&alone), vec![3.0]);
    }

    #[test]
    fn participation_windows_slide_and_charge_a_unit_once() {
        let p = Participation {
            max_share: 0.5,
            window_slots: 10,
            price_bps: 10.0,
        };
        // Two own trades of 4 in one window, others trade 2: the first has
        // 4 - 0.5 * 6 = 1 over; the second 8 - 0.5 * 10 = 3 over, less the 1
        // already charged.
        let in_window = [
            trade("phoenix", 1, 2.0, false),
            trade("phoenix", 2, 4.0, true),
            trade("phoenix", 3, 4.0, true),
        ];
        assert_eq!(p.excess(&in_window), vec![0.0, 1.0, 2.0]);
        // The same trades 10 slots apart never share a window: slot 2 is out
        // of the window that ends at slot 12.
        let apart = [
            trade("phoenix", 2, 4.0, true),
            trade("phoenix", 12, 4.0, true),
        ];
        assert_eq!(p.excess(&apart), vec![2.0, 2.0]);
        // One slot closer and the first is still in the window.
        let near = [
            trade("phoenix", 2, 4.0, true),
            trade("phoenix", 11, 4.0, true),
        ];
        assert_eq!(p.excess(&near), vec![2.0, 2.0]);
    }

    #[test]
    fn participation_is_priced_at_the_fill_not_a_mark() {
        let p = Participation {
            max_share: 0.25,
            window_slots: 10,
            price_bps: 10.0,
        };
        let mut dear = trade("phoenix", 5, 4.0, true);
        dear.price = 120.0;
        let report = p.report(&[dear], &["phoenix".to_string()]);
        assert_eq!(report.excess_base, 3.0);
        assert!((report.charge_usd - 3.0 * 120.0 * 0.001).abs() < 1e-9);
    }

    #[test]
    fn participation_counts_each_venue_apart_and_prices_the_excess() {
        let p = Participation {
            max_share: 0.5,
            window_slots: 10,
            price_bps: 10.0,
        };
        // Heavy on one venue, light on the other: only the first is over.
        let trades = [
            trade("phoenix", 1, 5.0, true),
            trade("openbook", 1, 5.0, false),
            trade("openbook", 2, 1.0, true),
        ];
        let venues = vec!["phoenix".to_string(), "openbook".to_string()];
        let report = p.report(&trades, &venues);
        assert_eq!(report.excess_base, 2.5);
        assert!((report.charge_usd - 2.5 * 100.0 * 0.001).abs() < 1e-9);
        assert_eq!(report.venues[0].traded_base, 5.0);
        assert_eq!(report.venues[0].excess_base, 2.5);
        assert_eq!(report.venues[1].own_base, 1.0);
        assert_eq!(report.venues[1].excess_base, 0.0);
    }

    #[test]
    fn net_scoring_gives_nop_zero_and_pays_a_round_trip() {
        let venues = vec!["phoenix".to_string(), "openbook".to_string()];
        let net = NetScoring {
            inventory_bps: 0.0,
            venue_position: None,
        };
        // 2500 held, price 100 -> 96: the account lost 10000, nop scores 0.
        let nop = net.report(2500.0, 2500.0, (100.0, 96.0), -10_000.0, &[], &venues);
        assert_eq!(nop.drift_usd, -10_000.0);
        assert_eq!(nop.pnl_usd(), 0.0);
        // Buy 10 at 95 on one venue, sell 10 at 97 on the other, close 96:
        // the account gains 20 on top of the drift.
        let fills = vec![
            ("openbook".to_string(), Side::Buy, 10.0),
            ("phoenix".to_string(), Side::Sell, 10.0),
        ];
        let trip = net.report(
            2500.0,
            2500.0,
            (100.0, 96.0),
            -10_000.0 + 20.0,
            &fills,
            &venues,
        );
        assert!((trip.pnl_usd() - 20.0).abs() < 1e-9);
        assert_eq!(trip.venue_net[0], ("phoenix".to_string(), -10.0));
    }

    #[test]
    fn net_scoring_prices_inventory_and_venue_positions() {
        let venues = vec!["phoenix".to_string(), "openbook".to_string()];
        let net = NetScoring {
            inventory_bps: 50.0,
            venue_position: Some(VenuePosition {
                limit: 5.0,
                price_bps: 100.0,
            }),
        };
        let fills = vec![("phoenix".to_string(), Side::Buy, 8.0)];
        let r = net.report(100.0, 108.0, (100.0, 100.0), 0.0, &fills, &venues);
        // 8 units off the start at 100, 50 bps: 4. Phoenix 3 over the limit, 100 bps: 3.
        assert!((r.inventory_charge_usd - 4.0).abs() < 1e-9);
        assert!((r.venue_charge_usd - 3.0).abs() < 1e-9);
        assert!((r.pnl_usd() + 7.0).abs() < 1e-9);
        assert!(
            NetScoring {
                inventory_bps: -1.0,
                venue_position: None
            }
            .validate()
            .is_err()
        );
    }
}
