//! What every rung reads of its mandate, and how it trades inside it.
//!
//! A rung adapts to the task it runs on through a [`Desk`]: the session's
//! own market, the sides and position range its mandate allows, the price
//! limits, whether taking may only reduce, and the job, if the task hands it
//! one ([`Goal`]). Every order a rung sends goes through the desk, which
//! keeps it on an allowed side, inside the position range, short of the
//! job's end and inside the price limits, in whole lots and ticks.

use mtx_kit::{Market, Orders, Session, SessionOrder, Side, venue::PhoenixVenue};
use mtx_probes::Allowed;

/// How far through the touch a taking order may fill, in basis points.
const TAKE_SLIPPAGE_BPS: f64 = 15.0;
/// The largest taking order, in base units: one that crosses much of the book
/// doesn't fit a transaction's compute budget.
const SLICE: f64 = 10.0;
/// Lot and tick when the session's venue doesn't say (Phoenix's usual).
const DEFAULT_LOT: f64 = 0.001;
const DEFAULT_TICK: f64 = 0.01;

/// What the session asks of the account.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Goal {
    /// No job: the rung trades its own view, on the sides the mandate allows.
    Free,
    /// Trade from the starting position to `end` by the session's end:
    /// `dir` is 1 to buy, -1 to sell.
    Execute { dir: f64, end: f64 },
}

/// The task's order on the session's own market (`MTX_ORDER`, read with
/// the kit's [`SessionOrder`]): 1 to buy, -1 to sell, and how much.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Order {
    pub dir: f64,
    pub quantity: f64,
}

impl Order {
    pub fn of(order: &SessionOrder) -> Self {
        Self {
            dir: match order.side {
                Side::Bid => 1.0,
                Side::Ask => -1.0,
            },
            quantity: order.quantity,
        }
    }
}

/// The session's market, mandate and job, as the rungs trade them.
#[derive(Debug, Clone, PartialEq)]
pub struct Desk {
    pub goal: Goal,
    /// The sides the mandate lets the account hold.
    pub allowed: Allowed,
    pub buy: bool,
    pub sell: bool,
    pub min_position: f64,
    pub max_position: f64,
    pub max_buy_price: Option<f64>,
    pub min_sell_price: Option<f64>,
    /// Taking may only bring the position toward zero.
    pub reduce_only: bool,
    pub lot: f64,
    pub tick: f64,
    /// The broker's haircut on what is left at the end, in basis points,
    /// where the task prices the remainder that way (`MTX_BROKER_HAIRCUT_BPS`).
    pub haircut_bps: Option<f64>,
    pub order: Option<Order>,
    pub start_slot: u64,
    pub end_slot: u64,
    pub start_oracle: f64,
    /// The size of the job, in base units, once the first step has shown the
    /// starting position.
    pub total: f64,
    pub(crate) begun: bool,
}

impl Default for Desk {
    fn default() -> Self {
        Self {
            goal: Goal::Free,
            allowed: Allowed::Both,
            buy: true,
            sell: true,
            min_position: 0.0,
            max_position: 0.0,
            max_buy_price: None,
            min_sell_price: None,
            reduce_only: false,
            lot: DEFAULT_LOT,
            tick: DEFAULT_TICK,
            haircut_bps: None,
            order: None,
            start_slot: 0,
            end_slot: 1,
            start_oracle: 0.0,
            total: 0.0,
            begun: false,
        }
    }
}

impl Desk {
    /// The desk for a session: its own market's allowance (the mandate's
    /// first instrument when none is named so), and the venue's lot and tick.
    pub fn of(session: &Session) -> Self {
        let universe = &session.mandate.universe;
        let allowance = universe
            .iter()
            .find(|a| a.instrument.symbol == session.symbol)
            .or_else(|| universe.first());
        let order = session
            .order()
            .ok()
            .flatten()
            .filter(|o| allowance.is_none_or(|a| o.instrument == a.instrument));
        let close_out = allowance.and_then(|a| session.close_outs().ok()?.haircut(&a.instrument));
        let mut desk = Self {
            start_slot: session.start_slot,
            end_slot: session.end_slot.max(session.start_slot + 1),
            start_oracle: session.start_oracle,
            max_position: session.cap,
            min_position: -session.cap,
            order: order.as_ref().map(Order::of),
            haircut_bps: session
                .broker_haircut()
                .ok()
                .flatten()
                .or(close_out)
                .map(|h| h.0),
            reduce_only: order.as_ref().is_some_and(|o| o.reduce_only),
            ..Self::default()
        };
        if let Some(a) = allowance {
            let allows = |side: &str| {
                a.sides
                    .as_ref()
                    .is_none_or(|sides| sides.iter().any(|s| s.to_string() == side))
            };
            desk.buy = allows("buy");
            desk.sell = allows("sell");
            desk.max_position = a.max_position;
            desk.min_position = a.min_position.unwrap_or(-a.max_position);
            desk.max_buy_price = a.max_buy_price;
            desk.min_sell_price = a.min_sell_price;
            desk.reduce_only |= a.taking.is_some();
            desk.allowed = Allowed::of_bounds(a.max_position, a.min_position, desk.buy, desk.sell);
        }
        if let Ok(venue) = PhoenixVenue::from_json(&session.venue)
            && let Ok(market) = venue.market(&session.symbol)
        {
            let lot = venue.lots_to_base_units(market, 1);
            let tick = venue.ticks_to_usd(market, 1);
            if lot.is_finite() && lot > 0.0 {
                desk.lot = lot;
            }
            if tick.is_finite() && tick > 0.0 {
                desk.tick = tick;
            }
        }
        desk
    }

    /// Fix the job from the first step's position: the task's order from
    /// where the account starts, or, on a mandate that may only sell (buy),
    /// the long (short) it starts with, to zero.
    pub fn begin(&mut self, start: f64) {
        if self.begun {
            return;
        }
        self.begun = true;
        let range = |end: f64| end.clamp(self.min_position, self.max_position);
        self.goal = match self.order {
            Some(order) => Goal::Execute {
                dir: order.dir,
                end: range(start + order.dir * order.quantity),
            },
            None if self.sell && !self.buy && start > self.lot => Goal::Execute {
                dir: -1.0,
                end: range(0.0),
            },
            None if self.buy && !self.sell && start < -self.lot => Goal::Execute {
                dir: 1.0,
                end: range(0.0),
            },
            None => Goal::Free,
        };
        if let Goal::Execute { end, .. } = self.goal {
            self.total = (end - start).abs();
        }
    }

    /// How far into the session `slot` is, from 0 to 1.
    pub fn progress(&self, slot: u64) -> f64 {
        let span = (self.end_slot - self.start_slot) as f64;
        (slot.saturating_sub(self.start_slot) as f64 / span).min(1.0)
    }

    /// What the job still has to trade, signed (0 without a job).
    pub fn remaining(&self, position: f64) -> f64 {
        match self.goal {
            Goal::Execute { dir, end } => {
                let left = end - position;
                if left * dir > 0.0 { left } else { 0.0 }
            }
            Goal::Free => 0.0,
        }
    }

    /// `size` in whole lots, toward zero.
    pub fn lots(&self, size: f64) -> f64 {
        let lots = (size.abs() / self.lot + 1e-9).floor();
        (lots * self.lot).copysign(size)
    }

    /// The signed trade from `position` toward `position + wanted` that the
    /// mandate allows: an allowed side, inside the position range, never past
    /// the job's end, in whole lots.
    pub fn clip(&self, position: f64, wanted: f64) -> f64 {
        if (wanted > 0.0 && !self.buy) || (wanted < 0.0 && !self.sell) || !wanted.is_finite() {
            return 0.0;
        }
        let mut to = (position + wanted).clamp(self.min_position, self.max_position);
        if let Goal::Execute { dir, end } = self.goal {
            to = if dir > 0.0 { to.min(end) } else { to.max(end) };
        }
        let trade = self.lots(to - position);
        if trade * wanted <= 0.0 { 0.0 } else { trade }
    }

    /// Whether a taker trade of `signed` from `position` opens or grows a
    /// position, which a reduce-only mandate forbids.
    fn opens(&self, position: f64, signed: f64) -> bool {
        (position + signed).abs() > position.abs() + 1e-9 || (position + signed) * position < -1e-12
    }

    /// Cross the spread for up to `wanted` (signed), sliced, at a limit
    /// `TAKE_SLIPPAGE_BPS` through the touch and inside the price limits. On
    /// a reduce-only mandate an opening trade rests at the touch instead.
    pub fn take(&self, market: &Market, orders: &mut Orders, wanted: f64) {
        let trade = self.clip(market.position.size, wanted);
        if trade == 0.0 {
            return;
        }
        if self.reduce_only && self.opens(market.position.size, trade) {
            let touch = if trade > 0.0 {
                market.best_bid
            } else {
                market.best_ask
            };
            if let Some(price) = touch {
                self.rest(market, orders, trade, price);
            }
            return;
        }
        let slip = TAKE_SLIPPAGE_BPS / 1e4;
        let limit = if trade > 0.0 {
            let Some(ask) = market.best_ask else { return };
            let limit = self.ceil_tick(ask * (1.0 + slip));
            let limit = self
                .max_buy_price
                .map_or(limit, |m| limit.min(self.floor_tick(m)));
            if limit < ask {
                return;
            }
            limit
        } else {
            let Some(bid) = market.best_bid else { return };
            let limit = self.floor_tick(bid * (1.0 - slip));
            let limit = self
                .min_sell_price
                .map_or(limit, |m| limit.max(self.ceil_tick(m)));
            if limit > bid {
                return;
            }
            limit
        };
        let mut left = trade.abs();
        while left >= self.lot {
            let size = self.lots(left.min(SLICE));
            if size < self.lot {
                break;
            }
            if trade > 0.0 {
                orders.buy(size, limit);
            } else {
                orders.sell(size, limit);
            }
            left -= size;
        }
    }

    /// Rest an order for up to `wanted` (signed) at `price`, post-only, the
    /// price rounded away from the touch and inside the price limits. Its size
    /// is clipped as a trade from the current position would be.
    pub fn rest(&self, market: &Market, orders: &mut Orders, wanted: f64, price: f64) {
        let size = self.clip(market.position.size, wanted);
        if size == 0.0 || !price.is_finite() || price <= 0.0 {
            return;
        }
        if size > 0.0 {
            let price = self.floor_tick(price);
            let price = self
                .max_buy_price
                .map_or(price, |m| price.min(self.floor_tick(m)));
            if market.best_ask.is_none_or(|a| price < a) {
                orders.bid(price, size.abs()).post_only();
            }
        } else {
            let price = self.ceil_tick(price);
            let price = self
                .min_sell_price
                .map_or(price, |m| price.max(self.ceil_tick(m)));
            if market.best_bid.is_none_or(|b| price > b) {
                orders.ask(price, size.abs()).post_only();
            }
        }
    }

    /// Trade toward `position`, crossing: a free rung's target.
    pub fn target(&self, market: &Market, orders: &mut Orders, position: f64) {
        let wanted = position - market.position.size;
        if wanted.abs() >= self.lot {
            self.take(market, orders, wanted);
        }
    }

    /// The broker's price for what is left, where the task has one: the oracle
    /// less the haircut on a sale, plus it on a purchase.
    pub fn broker_price(&self, oracle: f64, dir: f64) -> Option<f64> {
        self.haircut_bps.map(|h| oracle * (1.0 + dir * h / 1e4))
    }

    fn floor_tick(&self, price: f64) -> f64 {
        (price / self.tick + 1e-9).floor() * self.tick
    }

    fn ceil_tick(&self, price: f64) -> f64 {
        (price / self.tick - 1e-9).ceil() * self.tick
    }
}

#[cfg(test)]
mod tests {
    use mtx_kit::{Intent, Position};

    use super::*;

    fn desk(goal: Goal) -> Desk {
        Desk {
            goal,
            max_position: 10.0,
            min_position: -10.0,
            lot: 0.01,
            tick: 0.01,
            start_slot: 100,
            end_slot: 1100,
            begun: true,
            ..Desk::default()
        }
    }

    fn market(position: f64) -> Market {
        Market {
            position: Position {
                size: position,
                entry_usd: 0.0,
            },
            mark: 100.0,
            oracle: 100.0,
            best_bid: Some(99.95),
            best_ask: Some(100.05),
            ..Market::default()
        }
    }

    #[test]
    fn the_job_comes_from_the_order_or_a_one_sided_start() {
        let mut d = Desk {
            begun: false,
            order: Some(Order {
                dir: 1.0,
                quantity: 4.0,
            }),
            ..desk(Goal::Free)
        };
        d.min_position = 0.0;
        d.begin(0.0);
        assert_eq!(d.goal, Goal::Execute { dir: 1.0, end: 4.0 });
        assert_eq!(d.total, 4.0);

        let mut sell_only = Desk {
            begun: false,
            buy: false,
            min_position: 0.0,
            ..desk(Goal::Free)
        };
        sell_only.begin(7.0);
        assert_eq!(
            sell_only.goal,
            Goal::Execute {
                dir: -1.0,
                end: 0.0
            }
        );
        assert_eq!(sell_only.total, 7.0);

        let mut free = Desk {
            begun: false,
            ..desk(Goal::Free)
        };
        free.begin(3.0);
        assert_eq!(free.goal, Goal::Free);
        // The first step fixes it.
        free.begin(-5.0);
        assert_eq!(free.goal, Goal::Free);
    }

    #[test]
    fn a_trade_stays_on_its_side_in_range_and_short_of_the_end() {
        let sell = desk(Goal::Execute {
            dir: -1.0,
            end: 0.0,
        });
        assert_eq!(sell.clip(5.0, -2.0), -2.0);
        assert_eq!(sell.clip(1.5, -2.0), -1.5);
        assert_eq!(sell.clip(5.0, 1.0), 1.0);
        let no_buys = Desk { buy: false, ..sell };
        assert_eq!(no_buys.clip(5.0, 1.0), 0.0);
        let free = desk(Goal::Free);
        assert_eq!(free.clip(9.0, 3.0), 1.0);
        assert_eq!(free.clip(-9.995, -1.0), 0.0);
        assert_eq!(free.clip(0.0, 0.004), 0.0);
        assert_eq!(free.clip(0.0, f64::NAN), 0.0);
        assert!((free.clip(0.0, 0.019) - 0.01).abs() < 1e-12);
    }

    #[test]
    fn a_take_slices_and_keeps_inside_the_price_limits() {
        let d = desk(Goal::Free);
        let mut orders = Orders::new();
        d.take(&market(0.0), &mut orders, 10.0);
        let buys: Vec<_> = orders.intents().to_vec();
        assert_eq!(buys.len(), 1);
        let Intent::Market { size, limit, .. } = buys[0] else {
            panic!("{buys:?}")
        };
        assert_eq!(size, 10.0);
        assert!((100.05..=100.05 * 1.0016).contains(&limit), "{limit}");

        let capped = Desk {
            max_buy_price: Some(100.0),
            ..d.clone()
        };
        let mut orders = Orders::new();
        capped.take(&market(0.0), &mut orders, 1.0);
        assert!(orders.intents().is_empty(), "the ask is over the cap");

        let floor = Desk {
            min_sell_price: Some(99.97),
            ..d.clone()
        };
        let mut orders = Orders::new();
        floor.take(&market(0.0), &mut orders, -1.0);
        assert!(orders.intents().is_empty(), "the bid is under the floor");
    }

    #[test]
    fn a_reduce_only_mandate_opens_by_resting() {
        let d = Desk {
            reduce_only: true,
            ..desk(Goal::Free)
        };
        let mut orders = Orders::new();
        d.take(&market(0.0), &mut orders, 2.0);
        assert!(
            matches!(orders.intents(), [Intent::Limit { price, .. }] if (*price - 99.95).abs() < 1e-9)
        );
        let mut orders = Orders::new();
        d.take(&market(3.0), &mut orders, -2.0);
        assert!(matches!(orders.intents(), [Intent::Market { .. }]));
    }

    #[test]
    fn a_resting_order_never_crosses_and_rounds_away_from_the_touch() {
        let d = desk(Goal::Free);
        let mut orders = Orders::new();
        d.rest(&market(0.0), &mut orders, -1.0, 100.011);
        assert!(
            matches!(orders.intents(), [Intent::Limit { price, .. }] if (*price - 100.02).abs() < 1e-9)
        );
        let mut orders = Orders::new();
        d.rest(&market(0.0), &mut orders, 1.0, 100.10);
        assert!(orders.intents().is_empty(), "a bid over the ask would take");
    }

    #[test]
    fn the_broker_price_takes_the_haircut_off_a_sale() {
        let d = Desk {
            haircut_bps: Some(10.0),
            ..desk(Goal::Free)
        };
        assert!((d.broker_price(100.0, -1.0).unwrap() - 99.9).abs() < 1e-9);
        assert!((d.broker_price(100.0, 1.0).unwrap() - 100.1).abs() < 1e-9);
        assert_eq!(desk(Goal::Free).broker_price(100.0, 1.0), None);
        assert_eq!(desk(Goal::Free).progress(600), 0.5);
    }
}
