//! The anchor-strength hybrid: the task's informed bot, with a zoo rung
//! trading the steps it leaves idle.
//!
//! The informed bot is v2's 1: the best a trader told the task's facts can
//! do. If it leaves public edge on the table (a passive price it never rests
//! at, a free wait it never takes), a bot told the same facts that also
//! trades that edge beats it, and the scale's top is soft. The hybrid is
//! such a bot, built without knowing the task:
//!
//! - every step both strategies see the market as it is (the account's real
//!   position and orders);
//! - the informed bot goes first: whenever its sheet would trade or rest
//!   anything, or cancel orders it has resting, that sheet is sent alone,
//!   after cancelling whatever the zoo rung has resting;
//! - otherwise, unless the informed bot has orders resting, the zoo rung's
//!   sheet is sent.
//!
//! It is a policy any bot told the informed bot's facts could run: it never
//! looks ahead, and the zoo rung reads only public data. So when it beats the
//! informed bot by more than noise, a better told policy exists, whatever
//! the overlay's rule: the check can miss a weak anchor (the zoo rung never
//! finds an idle step, or the informed bot undoes what it did) but never
//! invents one. The informed bot sees the zoo's fills as its own position:
//! a bot that trades toward targets takes them back, which costs the hybrid
//! and can only hide a weakness.

use mtx_kit::{
    Action, BotResult, Intent, Market, Orders, Session, Strategy, TimeInForce, log, plan,
};
use mtx_probes::Boxed;

/// Who placed what rests on the book now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Nobody,
    Informed,
    Zoo,
}

pub struct Hybrid {
    informed: Boxed,
    zoo: Boxed,
    owner: Owner,
    /// Steps the zoo rung's sheet went out, and steps it traded or rested
    /// anything.
    zoo_steps: u64,
    zoo_acts: u64,
}

/// Whether `sheet` would trade or rest anything on `market`: a cancel alone
/// doesn't count.
fn acts(sheet: &Orders, market: &Market) -> bool {
    let (actions, _) = plan(sheet.intents(), market);
    actions
        .iter()
        .any(|a| !matches!(a, Action::CancelAll | Action::Cancel { .. }))
}

/// Whether `sheet` leaves orders resting.
fn rests(sheet: &Orders) -> bool {
    sheet.lines().any(|(intent, options)| match intent {
        Intent::Quote { .. } | Intent::AddLiquidity { .. } => true,
        Intent::Limit { .. } => options.time_in_force != TimeInForce::Ioc,
        _ => false,
    })
}

/// Whether `sheet` cancels resting orders.
fn cancels(sheet: &Orders) -> bool {
    sheet.intents().iter().any(|i| {
        matches!(
            i,
            Intent::CancelAll | Intent::Cancel { .. } | Intent::Quote { .. }
        )
    })
}

fn send(sheet: Orders, orders: &mut Orders) {
    for (intent, options) in sheet.into_lines() {
        orders.push(intent, options);
    }
}

impl Hybrid {
    pub fn new(informed: Box<dyn Strategy>, zoo: Box<dyn Strategy>) -> Self {
        Self {
            informed: Boxed(informed),
            zoo: Boxed(zoo),
            owner: Owner::Nobody,
            zoo_steps: 0,
            zoo_acts: 0,
        }
    }

    /// Whose orders rest after `sheet` goes out, from `before`.
    fn after(before: Owner, sheet: &Orders, by: Owner) -> Owner {
        if rests(sheet) {
            by
        } else if cancels(sheet) {
            Owner::Nobody
        } else {
            before
        }
    }
}

impl Strategy for Hybrid {
    fn start(&mut self, session: &Session) -> BotResult {
        self.informed.start(session)?;
        // The zoo rung reads only what any bot sees: never the told state.
        let mut public = session.clone();
        public.vars.retain(|(name, _)| name != "MTX_REGIME");
        self.zoo.start(&public)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let mut informed = Orders::new();
        self.informed.step(market, &mut informed)?;
        let mut zoo = Orders::new();
        let zoo_ok = self.zoo.step(market, &mut zoo).is_ok();
        let informed_acts =
            acts(&informed, market) || (self.owner == Owner::Informed && cancels(&informed));
        if informed_acts {
            let before = if self.owner == Owner::Zoo {
                orders.cancel_all();
                Owner::Nobody
            } else {
                self.owner
            };
            self.owner = Self::after(before, &informed, Owner::Informed);
            send(informed, orders);
        } else if self.owner != Owner::Informed && zoo_ok {
            self.zoo_steps += 1;
            if acts(&zoo, market) {
                self.zoo_acts += 1;
            }
            self.owner = Self::after(self.owner, &zoo, Owner::Zoo);
            send(zoo, orders);
        }
        Ok(())
    }

    fn end(&mut self, last: Option<&Market>) {
        self.informed.end(last);
        self.zoo.end(last);
        log(&format!(
            "hybrid: the zoo rung had {} idle steps and traded or rested in {}",
            self.zoo_steps, self.zoo_acts
        ));
    }
}

#[cfg(test)]
mod tests {
    use mtx_kit::{Position, Side};

    use super::*;

    /// A strategy that plays a script of sheets, one a step.
    struct Script(Vec<fn(&mut Orders)>, usize);

    impl Strategy for Script {
        fn step(&mut self, _: &Market, orders: &mut Orders) -> BotResult {
            if let Some(f) = self.0.get(self.1) {
                f(orders);
            }
            self.1 += 1;
            Ok(())
        }
    }

    fn market() -> Market {
        Market {
            position: Position {
                size: 5.0,
                entry_usd: 0.0,
            },
            mark: 100.0,
            oracle: 100.0,
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            cap: 10.0,
            ..Market::default()
        }
    }

    fn run(informed: Vec<fn(&mut Orders)>, zoo: Vec<fn(&mut Orders)>) -> Vec<Vec<Intent>> {
        let steps = informed.len().max(zoo.len());
        let mut h = Hybrid::new(Box::new(Script(informed, 0)), Box::new(Script(zoo, 0)));
        (0..steps)
            .map(|_| {
                let mut orders = Orders::new();
                h.step(&market(), &mut orders).unwrap();
                orders.intents().to_vec()
            })
            .collect()
    }

    fn idle(o: &mut Orders) {
        o.cancel_all();
    }

    fn hold(o: &mut Orders) {
        o.target_position(5.0);
    }

    fn sell(o: &mut Orders) {
        o.sell(1.0, 99.0);
    }

    fn rest_ask(o: &mut Orders) {
        o.ask(100.5, 1.0).post_only();
    }

    #[test]
    fn the_informed_bot_goes_first_and_the_zoo_fills_its_idle_steps() {
        let out = run(vec![idle, sell, hold], vec![rest_ask, rest_ask, rest_ask]);
        // Idle (a cancel alone, a target it already holds): the zoo's sheet.
        assert!(matches!(
            out[0][..],
            [Intent::Limit {
                side: Side::Ask,
                ..
            }]
        ));
        // The informed bot trades: the zoo's resting ask is cancelled first.
        assert!(matches!(
            out[1][..],
            [
                Intent::CancelAll,
                Intent::Market {
                    side: Side::Ask,
                    ..
                }
            ]
        ));
        assert!(matches!(
            out[2][..],
            [Intent::Limit {
                side: Side::Ask,
                ..
            }]
        ));
    }

    #[test]
    fn the_zoo_never_touches_what_the_informed_bot_rests() {
        let out = run(
            vec![rest_ask, hold, idle, hold],
            vec![sell, sell, sell, sell],
        );
        assert!(matches!(out[0][..], [Intent::Limit { .. }]));
        // The informed bot's ask rests: the zoo waits.
        assert!(out[1].is_empty());
        // Its cancel is its own act: sent, and the book is the zoo's again.
        assert!(matches!(out[2][..], [Intent::CancelAll]));
        assert!(matches!(out[3][..], [Intent::Market { .. }]));
    }

    #[test]
    fn the_sheet_readers_tell_trading_resting_and_cancelling_apart() {
        let m = market();
        let mut o = Orders::new();
        o.cancel_all().target_position(5.0);
        assert!(!acts(&o, &m) && cancels(&o) && !rests(&o));
        let mut o = Orders::new();
        o.bid(99.0, 1.0).ioc();
        assert!(acts(&o, &m) && !rests(&o));
        let mut o = Orders::new();
        o.quote(99.0, 101.0, 1.0);
        assert!(acts(&o, &m) && rests(&o) && cancels(&o));
    }
}
