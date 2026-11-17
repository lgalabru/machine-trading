//! A trader that minds being copied: it watches who trades its way right
//! after its own fills, as anyone reading the chain can, and once a single
//! account's flow keeps following it, it hides its trading for a while.
//!
//! Over its last `fills` fills, it adds up each other account's flow that
//! came within `detect_slots` after one of them, on that fill's side, less
//! its flow on that side within `detect_slots` before one: a copier trades
//! right after it, while a trader on the same news as likely trades before
//! as after. An account whose net following flow comes to `copy_share` of
//! what those fills traded is copying it. It then trades `throttle` of its
//! usual size, `throttle` as often, and looks only `throttle` of its
//! horizon ahead, so it trades later, closer to the move, until
//! `cooldown_slots` after the last time it found itself copied. Each finding
//! starts its count of fills afresh.
//!
//! The accounts are the library's other wallets, through the takes they
//! land, and the bot, through its position from step to step (the session's
//! [`StepFlow`]): what it bought or sold in a step, takes and fills alike.

use std::collections::{BTreeMap, HashSet, VecDeque};

use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use super::{Ledger, finite_positive};
use crate::actors::StepFlow;

fn three() -> u32 {
    3
}

/// How a trader reacts to being copied (see the module).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Shy {
    /// How long after its last fill another account's flow its way still
    /// counts as following it, in slots.
    pub detect_slots: u64,
    /// The share of its own size, over its last `fills` fills, one other
    /// account's following flow must reach.
    pub copy_share: f64,
    /// How many of its latest fills it judges over.
    #[serde(default = "three")]
    pub fills: u32,
    /// Once copied, the share of its size and of its horizon it keeps.
    pub throttle: f64,
    /// How long it stays throttled after it last found itself copied.
    pub cooldown_slots: u64,
}

impl Shy {
    pub fn validate(&self) -> Result<(), String> {
        if self.detect_slots == 0 {
            return Err("shy.detect_slots must be at least 1".into());
        }
        if self.fills == 0 {
            return Err("shy.fills must be at least 1".into());
        }
        if self.cooldown_slots == 0 {
            return Err("shy.cooldown_slots must be at least 1".into());
        }
        finite_positive("shy.copy_share", self.copy_share)?;
        finite_positive("shy.throttle", self.throttle)?;
        if self.throttle > 1.0 {
            return Err("shy.throttle must be at most 1".into());
        }
        Ok(())
    }
}

/// Who traded: one of the library's wallets, or the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Who {
    Wallet(Pubkey),
    Bot,
}

/// Who a shy trader found copying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Copier {
    Wallet,
    Bot,
}

/// One account's trading one way at a slot into the session.
#[derive(Debug, Clone, Copy)]
struct Traded {
    at: u64,
    who: Who,
    side: Side,
    size: f64,
}

/// A shy trader's watch on who follows it.
pub(super) struct ShyWatch {
    spec: Shy,
    own: HashSet<Pubkey>,
    ledger: Ledger,
    /// How many of the ledger's takes it has read.
    read: usize,
    /// Its own fills since it last found itself copied, the latest
    /// `spec.fills` of them.
    fills: VecDeque<(u64, Side, f64)>,
    /// Everyone else's trading since the first of those fills.
    others: Vec<Traded>,
    /// The slot the current step's takes are made at.
    step: u64,
    /// Throttled until this slot.
    until: Option<u64>,
}

impl ShyWatch {
    /// A watch for the trader whose wallets are `own`, reading the other
    /// wallets' takes from `ledger`.
    pub fn new(spec: Shy, own: HashSet<Pubkey>, ledger: Ledger) -> Self {
        Self {
            spec,
            own,
            ledger,
            read: 0,
            fills: VecDeque::new(),
            others: Vec::new(),
            step: 0,
            until: None,
        }
    }

    /// The start of a step at `at`: read who traded since the last, and
    /// judge whether it is copied. Whether it found itself copied now, and
    /// whether by the bot.
    pub fn begin(&mut self, at: u64) -> Option<Copier> {
        self.step = at;
        let takes = self.ledger.takes_from(self.read);
        self.read += takes.len();
        for (at, wallet, side, size) in takes {
            if !self.own.contains(&wallet) {
                self.push(Traded {
                    at,
                    who: Who::Wallet(wallet),
                    side,
                    size,
                });
            }
        }
        let copier = self.copier()?;
        self.until = Some(at + self.spec.cooldown_slots);
        self.fills.clear();
        self.others.clear();
        Some(match copier {
            Who::Bot => Copier::Bot,
            Who::Wallet(_) => Copier::Wallet,
        })
    }

    /// The bot's trading over the step, from its position.
    pub fn bot(&mut self, flow: &StepFlow) {
        for side in [Side::Bid, Side::Ask] {
            let size = flow.bot_traded(side);
            if size > 0.0 {
                self.push(Traded {
                    at: self.step,
                    who: Who::Bot,
                    side,
                    size,
                });
            }
        }
    }

    /// One of its own takes landed: `size` on `side` at `at`.
    pub fn filled(&mut self, at: u64, side: Side, size: f64) {
        // Takes of one step, one way, are one fill: what anyone reading the
        // chain step by step sees.
        if let Some(last) = self.fills.back_mut()
            && last.0 == at
            && last.1 == side
        {
            last.2 += size;
            return;
        }
        self.fills.push_back((at, side, size));
        while self.fills.len() > self.spec.fills as usize {
            self.fills.pop_front();
        }
        self.prune();
    }

    /// Whether it is throttled at `at`.
    pub fn throttled(&self, at: u64) -> bool {
        self.until.is_some_and(|until| at < until)
    }

    /// What it keeps of its size and horizon at `at`: all of them, or the
    /// throttle's share.
    pub fn keep(&self, at: u64) -> f64 {
        if self.throttled(at) {
            self.spec.throttle
        } else {
            1.0
        }
    }

    fn push(&mut self, traded: Traded) {
        self.others.push(traded);
        self.prune();
    }

    /// Forget trading too old to fall near any fill it judges by, or that
    /// may come.
    fn prune(&mut self) {
        let from = self.fills.front().map_or(self.step, |(first, ..)| *first);
        let detect = self.spec.detect_slots;
        self.others.retain(|t| t.at + detect >= from);
    }

    /// The account whose flow right after its last `fills` fills, less its
    /// flow right before them, came to `copy_share` of what they traded, if
    /// one did (the bot first).
    fn copier(&self) -> Option<Who> {
        if self.fills.len() < self.spec.fills as usize {
            return None;
        }
        let own: f64 = self.fills.iter().map(|(.., size)| size).sum();
        let detect = self.spec.detect_slots;
        let near = |traded: &Traded, after: bool| {
            self.fills.iter().any(|(at, side, _)| {
                *side == traded.side
                    && if after {
                        *at < traded.at && traded.at <= at + detect
                    } else {
                        traded.at < *at && traded.at + detect >= *at
                    }
            })
        };
        let mut followed = BTreeMap::<Who, f64>::new();
        for traded in &self.others {
            let lead = f64::from(u8::from(near(traded, true))) - f64::from(u8::from(near(traded, false)));
            *followed.entry(traded.who).or_default() += lead * traded.size;
        }
        followed
            .into_iter()
            .rev()
            .find(|(_, size)| *size >= self.spec.copy_share * own)
            .map(|(who, _)| who)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{VenueFlow, library::Ledger};

    fn spec() -> Shy {
        Shy {
            detect_slots: 4,
            copy_share: 0.5,
            fills: 3,
            throttle: 0.2,
            cooldown_slots: 100,
        }
    }

    fn bot_bought(size: f64) -> StepFlow {
        StepFlow {
            bot: vec![VenueFlow {
                market: "test".into(),
                bought: size,
                sold: 0.0,
            }],
        }
    }

    /// Three own fills of 1 at slots 10, 20 and 30, each followed by the
    /// bot buying `copy` two slots later.
    fn followed_by_bot(copy: f64) -> (ShyWatch, bool) {
        let mut watch = ShyWatch::new(spec(), HashSet::new(), Ledger::with_takes());
        let mut found = false;
        for at in (10..=40).step_by(2) {
            found |= watch.begin(at).is_some();
            if [12, 22, 32].contains(&at) {
                watch.bot(&bot_bought(copy));
            }
            if [10, 20, 30].contains(&at) {
                watch.filled(at, Side::Bid, 1.0);
            }
        }
        (watch, found)
    }

    #[test]
    fn a_follower_past_the_share_throttles_it_and_one_under_does_not() {
        let (watch, found) = followed_by_bot(0.6);
        assert!(found);
        assert!(watch.throttled(40) && watch.keep(40) == 0.2);
        assert!(watch.throttled(133) && !watch.throttled(134));
        let (watch, found) = followed_by_bot(0.4);
        assert!(!found && !watch.throttled(40) && watch.keep(40) == 1.0);
    }

    #[test]
    fn only_one_account_s_flow_right_after_its_fills_counts() {
        let ledger = Ledger::with_takes();
        let (a, b, c) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let own = Pubkey::new_unique();
        let mut watch = ShyWatch::new(spec(), HashSet::from([own]), ledger.clone());
        watch.begin(8);
        for at in [10, 20, 30] {
            watch.filled(at, Side::Ask, 1.0);
        }
        // `a` and `b` each sell 0.9 right after its fills: under half of 3
        // each. `c` sells as much right before them as right after: on the
        // same news, not following. Its own wallets, the other side, the
        // slot of a fill and anything past the window don't count.
        for (at, who, side, size) in [
            (12, a, Side::Ask, 0.5),
            (14, b, Side::Ask, 0.5),
            (22, a, Side::Ask, 0.4),
            (24, b, Side::Ask, 0.4),
            (18, c, Side::Ask, 2.0),
            (22, c, Side::Ask, 2.0),
            (22, own, Side::Ask, 5.0),
            (22, a, Side::Bid, 5.0),
            (20, b, Side::Ask, 5.0),
            (36, a, Side::Ask, 5.0),
        ] {
            ledger.took(at, who, side, size);
        }
        assert_eq!(watch.begin(38), None);
        // One more sale right after a fill makes a copier of `a`.
        ledger.took(32, a, Side::Ask, 0.7);
        assert_eq!(watch.begin(40), Some(Copier::Wallet));
        assert!(watch.throttled(40) && watch.throttled(139) && !watch.throttled(140));
    }

    #[test]
    fn a_finding_starts_the_count_afresh_and_copying_while_throttled_extends_it() {
        let (mut watch, found) = followed_by_bot(1.0);
        assert!(found && watch.fills.is_empty());
        // Copied again while throttled: the cooldown runs from the new
        // finding.
        for at in [50, 60, 70] {
            watch.begin(at);
            watch.filled(at, Side::Bid, 0.2);
        }
        watch.begin(72);
        watch.bot(&bot_bought(1.0));
        assert_eq!(watch.begin(74), Some(Copier::Bot));
        assert!(watch.throttled(173) && !watch.throttled(174));
    }

    #[test]
    fn a_shy_setting_is_checked() {
        assert!(spec().validate().is_ok());
        for bad in [
            Shy {
                detect_slots: 0,
                ..spec()
            },
            Shy {
                copy_share: 0.0,
                ..spec()
            },
            Shy {
                throttle: 1.5,
                ..spec()
            },
            Shy {
                throttle: f64::NAN,
                ..spec()
            },
            Shy { fills: 0, ..spec() },
            Shy {
                cooldown_slots: 0,
                ..spec()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
