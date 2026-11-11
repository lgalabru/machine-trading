//! Execution orders and the participation charge: what a spot scenario can
//! ask of the bot beyond making money, and what it costs to break.
//!
//! An [`ExecutionOrder`] is a quantity the bot has to trade by the end of
//! the session, one way: sell it (a treasury working a position off), or buy
//! it (a treasury building one). Whatever it hasn't traded by then, a broker
//! trades for it at the session's closing price, worse by the order's
//! haircut. The session's PnL is the bot's against doing nothing, so a bot
//! that never trades scores 0, and each unit it trades is worth the haircut
//! it saved: `PnL = value change + haircut × close × done`.
//!
//! Trading the other way is a breach, and so is, for a `reduce_only` order,
//! any fill that grows the position. Neither is in the mandate, which has no
//! "sell by the end": the order belongs to the scenario's scoring.
//!
//! [`Participation`] prices taking too much of a venue's volume. It is a
//! charge in the PnL, not a rule: a bot that trades big pays for it.

use mtx_core::Side;
use serde::{Deserialize, Serialize};

/// A quantity to trade by the end of the session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOrder {
    /// `sell` to get rid of the quantity, `buy` to acquire it.
    pub side: Side,
    /// Base units the bot is to trade by the end.
    pub quantity: f64,
    /// What the broker's price for the remainder is worse than the close by,
    /// in basis points: lower when it sells, higher when it buys.
    pub haircut_bps: f64,
    /// Whether any fill that grows the position is a breach.
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
        if self.reduce_only && self.side == Side::Buy {
            return Err("order: a buy order can't be reduce-only on a spot wallet".into());
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
    /// are the base units the wallet held, `value_change_usd` the account's
    /// change in value at the close, and `fills` the bot's own fills
    /// (side, base units), oldest first.
    pub fn report(
        &self,
        start_base: f64,
        end_base: f64,
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
            side: self.side,
            quantity: self.quantity,
            done: moved.clamp(0.0, self.quantity),
            close_usd,
            broker_usd: self.broker_usd(close_usd),
            value_change_usd,
            against,
            grown: if self.reduce_only { grown } else { 0.0 },
        }
    }
}

/// An order session's account of the order. The session's PnL is the bot's
/// against doing nothing, which the broker would have done at its price.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OrderReport {
    pub side: Side,
    pub quantity: f64,
    /// Base units traded toward the order by the end, at most the quantity.
    pub done: f64,
    /// The session's closing price, in USD, and the broker's price for the
    /// remainder.
    pub close_usd: f64,
    pub broker_usd: f64,
    /// What the account's value changed by over the session, valued at the
    /// close; the PnL adds the haircut on each unit done.
    pub value_change_usd: f64,
    /// Base units the bot traded the other way, over the session.
    pub against: f64,
    /// Base units by which its fills grew the position (reduce-only orders).
    pub grown: f64,
}

impl OrderReport {
    /// The PnL against the broker: each unit done is worth what the broker
    /// would have taken off it.
    pub fn pnl_usd(&self) -> f64 {
        self.value_change_usd + self.done * (self.broker_usd - self.close_usd).abs()
    }

    /// Why the session breaks the order's rules, if it does.
    pub fn breach(&self) -> Option<String> {
        let round = |x: f64| (x * 1000.0).round() / 1000.0;
        if self.against > 1e-6 {
            let (did, only) = match self.side {
                Side::Sell => ("bought", "sells"),
                Side::Buy => ("sold", "buys"),
            };
            Some(format!(
                "{did} {} SOL: an order to {} only {only}",
                round(self.against),
                match self.side {
                    Side::Sell => "sell",
                    Side::Buy => "buy",
                }
            ))
        } else if self.grown > 1e-6 {
            Some(format!(
                "grew the position by {} SOL: the order is reduce-only",
                round(self.grown)
            ))
        } else {
            None
        }
    }
}

/// A limit on the bot's share of a venue's traded volume, priced: base units
/// over it cost `price_bps` of the mark.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Participation {
    /// The most of a venue's traded base volume, in any window, that is the
    /// bot's before the charge starts: a share in (0, 1].
    pub max_share: f64,
    /// The window's length, in slots: it ends at each of the bot's fills.
    pub window_slots: u64,
    /// The charge on each base unit over the share, in basis points of the
    /// mark.
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
    /// Slots into the session.
    pub slot: u64,
    pub base: f64,
    /// Whether the bot was one side of it.
    pub own: bool,
}

/// What the participation charge came to.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ParticipationReport {
    /// Base units charged, in all, and the charge in USD, which the PnL
    /// is net of.
    pub excess_base: f64,
    pub charge_usd: f64,
    /// Each venue's traded base volume over the session, and the bot's part
    /// of it.
    pub venues: Vec<VenueShare>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VenueShare {
    pub venue: String,
    pub traded_base: f64,
    pub own_base: f64,
    pub excess_base: f64,
}

impl Participation {
    /// The charge for a session's trades, at `mark_usd`.
    pub fn report(
        &self,
        trades: &[Traded],
        venues: &[String],
        mark_usd: f64,
    ) -> ParticipationReport {
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
            charge_usd: excess_base * mark_usd * self.price_bps / 10_000.0,
            venues: shares,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: Side) -> ExecutionOrder {
        ExecutionOrder {
            side,
            quantity: 10.0,
            haircut_bps: 100.0,
            reduce_only: false,
        }
    }

    fn trade(venue: &str, slot: u64, base: f64, own: bool) -> Traded {
        Traded {
            venue: venue.into(),
            slot,
            base,
            own,
        }
    }

    #[test]
    fn doing_nothing_scores_zero_and_each_unit_done_earns_the_haircut() {
        let sell = order(Side::Sell);
        let nothing = sell.report(10.0, 10.0, 100.0, 0.0, &[]);
        assert_eq!(nothing.done, 0.0);
        assert_eq!(nothing.pnl_usd(), 0.0);
        assert_eq!(nothing.breach(), None);
        // Sold 4 at 100.5: 402 in, 400 out at the close, 1% off the close
        // the broker takes: 4 units save 4.
        let sold = sell.report(10.0, 6.0, 100.0, 4.0 * 100.5 - 400.0, &[(Side::Sell, 4.0)]);
        assert_eq!(sold.done, 4.0);
        assert!((sold.pnl_usd() - (2.0 + 4.0)).abs() < 1e-9);
        assert_eq!(sold.broker_usd, 99.0);
        // Selling past the quantity counts the quantity.
        assert_eq!(sell.report(20.0, 0.0, 100.0, 0.0, &[]).done, 10.0);
    }

    #[test]
    fn the_buy_order_mirrors_the_sale() {
        let buy = order(Side::Buy);
        assert_eq!(buy.broker_usd(100.0), 101.0);
        // Bought 4 at 99.5: 398 out, worth 400 at the close; the broker
        // would have charged 404 for them.
        let report = buy.report(0.0, 4.0, 100.0, 400.0 - 398.0, &[(Side::Buy, 4.0)]);
        assert_eq!(report.done, 4.0);
        assert!((report.pnl_usd() - 6.0).abs() < 1e-9);
        assert_eq!(buy.report(0.0, 0.0, 100.0, 0.0, &[]).pnl_usd(), 0.0);
        // Selling what it holds is the wrong way for a buy.
        let sold = buy.report(5.0, 4.0, 100.0, 0.0, &[(Side::Sell, 1.0)]);
        assert!(sold.breach().unwrap().contains("only buys"));
    }

    #[test]
    fn trading_the_other_way_is_a_breach_even_if_it_nets_out() {
        let fills = [(Side::Sell, 3.0), (Side::Buy, 0.5), (Side::Sell, 3.0)];
        let report = order(Side::Sell).report(10.0, 3.5, 100.0, 0.0, &fills);
        assert_eq!(report.against, 0.5);
        assert!(report.breach().unwrap().contains("only sells"));
    }

    #[test]
    fn reduce_only_breaches_a_fill_that_grows_the_position() {
        let mut sell = order(Side::Sell);
        sell.reduce_only = true;
        // Long 2 and selling 5 flips it short 3: the position grew past
        // flat by 1.
        let flipped = sell.report(2.0, -3.0, 100.0, 0.0, &[(Side::Sell, 5.0)]);
        assert_eq!(flipped.grown, 3.0 - 2.0);
        assert!(flipped.breach().unwrap().contains("reduce-only"));
        let reduced = sell.report(5.0, 1.0, 100.0, 0.0, &[(Side::Sell, 4.0)]);
        assert_eq!(reduced.breach(), None);
        // Not asked for: not reported.
        let plain = order(Side::Sell).report(2.0, -3.0, 100.0, 0.0, &[(Side::Sell, 5.0)]);
        assert_eq!(plain.grown, 0.0);
        let mut buy = order(Side::Buy);
        buy.reduce_only = true;
        assert!(buy.validate().is_err());
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
        let report = p.report(&trades, &venues, 150.0);
        assert_eq!(report.excess_base, 2.5);
        assert!((report.charge_usd - 2.5 * 150.0 * 0.001).abs() < 1e-9);
        assert_eq!(report.venues[0].traded_base, 5.0);
        assert_eq!(report.venues[0].excess_base, 2.5);
        assert_eq!(report.venues[1].own_base, 1.0);
        assert_eq!(report.venues[1].excess_base, 0.0);
    }
}
