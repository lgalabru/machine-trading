//! What a host measures of a session it runs a strategy in: the account at
//! each step, what traded, and what became of each send. Plain records,
//! shared by every host (the grader, the runtime that trades on mainnet)
//! and by what reads their reports.

use serde::{Deserialize, Serialize};

/// The account at a step, as the session measures it (not the strategy's
/// estimate): taken before the strategy's turn, so the position change is
/// what filled during the previous step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Account {
    pub value_usd: f64,
    pub pnl_usd: f64,
    /// Below the session's highest value so far; in a net-scored session,
    /// net of what holding the start would have gained.
    pub drawdown_usd: f64,
    pub position: f64,
    /// Since the step before: the net of what filled meanwhile.
    pub position_change: f64,
    /// The scenario's planned level at the step.
    pub level_usd: f64,
    /// How far trades have the book from the scenario's level, in basis
    /// points, where the scenario has an `impact`.
    pub impact_bps: Option<f64>,
    /// What traded on the market since the step before, between anyone,
    /// where the session reads it.
    pub trades: Option<StepTrades>,
    /// The price the session values the position at, where it isn't
    /// Phoenix's mark: market making's oracle level, or the book's level
    /// where the book trades at a premium.
    pub valued_at_usd: Option<f64>,
}

/// What traded on the session's market in a step, between anyone: read from
/// the fills the market's transactions recorded, each match once. A chart's
/// candles take their highs, lows and volume from it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct StepTrades {
    /// Matches.
    pub count: u64,
    /// Base units traded, and what they traded for, in USD.
    pub base: f64,
    pub quote_usd: f64,
    /// Of `base`, what takers bought; the rest they sold.
    pub bought_base: f64,
    /// The first and last match's price, and the highest and lowest. All 0
    /// when nothing traded.
    pub open_usd: f64,
    pub high_usd: f64,
    pub low_usd: f64,
    pub close_usd: f64,
}

impl StepTrades {
    /// Count one match: `base` units at `price`, the taker buying or not.
    pub fn add(&mut self, price: f64, base: f64, taker_bought: bool) {
        if self.count == 0 {
            (self.open_usd, self.high_usd, self.low_usd) = (price, price, price);
        }
        self.count += 1;
        self.base += base;
        self.quote_usd += base * price;
        if taker_bought {
            self.bought_base += base;
        }
        self.high_usd = self.high_usd.max(price);
        self.low_usd = self.low_usd.min(price);
        self.close_usd = price;
    }
}

/// What the account holds, and what it's worth.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Wallet {
    /// SOL and USDC, in the wallet and on every venue, free or locked.
    pub base: f64,
    pub quote: f64,
    /// The Pyth SOL/USD price it was valued at, and its value.
    pub price_usd: f64,
    pub value_usd: f64,
    /// A lending session's reserves, as a component's step shows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub venues: Vec<VenueReading>,
}

/// One reserve of a measured account, as a component's step shows it: the
/// supply and debt there, and the reserve's numbers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VenueReading {
    pub id: String,
    pub supplied: f64,
    pub borrowed: f64,
    pub health: Option<f64>,
    pub stats: Vec<(String, f64)>,
}

impl Wallet {
    /// Value the wallet at `price` if it is one, as the valuation feed's
    /// anchor makes the start: the held SOL is worth what the feed will say
    /// from the first slot, so the shift isn't PnL.
    pub fn reprice(&mut self, price: Option<f64>) {
        if let Some(price) = price {
            self.value_usd += self.base * (price - self.price_usd);
            self.price_usd = price;
        }
    }
}

/// What became of a send, as a status reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum Fate {
    Pending,
    Landed {
        slot: u64,
        ok: bool,
        fee: u64,
        tip: u64,
        error: Option<String>,
        signature: String,
    },
    Failed(String),
    Expired,
}
