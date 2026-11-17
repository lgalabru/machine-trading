//! A trader that turns its wallet into bait once it sees it is copied: it
//! watches who trades its way right after it starts each run of trading,
//! as anyone reading the chain can, and once a single account keeps doing
//! so it stops trading on what it knows from that wallet. The wallet keeps
//! its pace, its sizes and its timing, but its views are now no better
//! than `hit_rate_after`: a copier pays for every run it copies from then
//! on, and nothing about the wallet's trading says when it turned.
//!
//! Its trading comes in runs (episodes): a run starts with its first take
//! after `horizon_slots` from the last run's start, or on the other side,
//! and holds the run's side. For each run, every other account's net flow
//! on the run's side within `follow_slots` after the run's start is set
//! against what the run's first `ref_fills` takes traded: an account whose
//! net following flow comes to `match_share` of that matched the run. Once
//! one account has matched `matches` of its last `episodes` runs, the bait
//! is set, for the rest of the session.
//!
//! A run is judged once its window has passed and its first `ref_fills`
//! takes are in, or once the run is over, whichever comes first. One run
//! is one piece of evidence: a single heavy take by an account that trades
//! at random matches a run now and then, and only an account that keeps
//! following run after run reaches `matches`.
//!
//! The accounts are the library's other wallets, through the takes they
//! land, and the bot, through its position from step to step (the session's
//! [`StepFlow`]): what it bought or sold in a step, takes and fills alike.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use super::{Ledger, finite_positive, within};
use crate::actors::StepFlow;

fn eight() -> u32 {
    8
}

fn three() -> u32 {
    3
}

fn half() -> f64 {
    0.5
}

/// How a trader that minds being copied turns its wallet into bait (see
/// the module).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Bait {
    /// How long after a run's start another account's flow its way counts
    /// as following it, in slots.
    pub follow_slots: u64,
    /// The share of the run's first takes one account's net following flow
    /// must come to for it to match the run.
    pub match_share: f64,
    /// How many of its last `episodes` runs one account must match.
    pub matches: u32,
    /// How many of its latest runs it judges over.
    #[serde(default = "eight")]
    pub episodes: u32,
    /// How many of a run's first takes it measures following flow against.
    #[serde(default = "three")]
    pub ref_fills: u32,
    /// Once the bait is set, the chance each of its views is right: 0.5 is
    /// a coin flip.
    #[serde(default = "half")]
    pub hit_rate_after: f64,
}

impl Bait {
    pub fn validate(&self) -> Result<(), String> {
        if self.follow_slots == 0 {
            return Err("bait.follow_slots must be at least 1".into());
        }
        if self.episodes == 0 {
            return Err("bait.episodes must be at least 1".into());
        }
        if self.matches == 0 || self.matches > self.episodes {
            return Err("bait.matches must be between 1 and bait.episodes".into());
        }
        if self.ref_fills == 0 {
            return Err("bait.ref_fills must be at least 1".into());
        }
        finite_positive("bait.match_share", self.match_share)?;
        within("bait.hit_rate_after", self.hit_rate_after, 0.5, 1.0)
    }
}

/// Who traded: one of the library's wallets, or the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Who {
    Wallet(Pubkey),
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

/// One of its runs of trading.
#[derive(Debug, Clone)]
struct Run {
    start: u64,
    side: Side,
    /// Its first takes' sizes, up to `ref_fills`.
    fills: Vec<f64>,
}

/// A baiting trader's watch on who follows its runs.
pub(super) struct BaitWatch {
    spec: Bait,
    horizon: u64,
    own: HashSet<Pubkey>,
    ledger: Ledger,
    /// How many of the ledger's takes it has read.
    read: usize,
    /// Runs not judged yet, oldest first.
    open: VecDeque<Run>,
    /// The latest run, judged or not: what a take of its own extends.
    latest: Option<Run>,
    /// The accounts that matched each of its last `episodes` judged runs.
    judged: VecDeque<BTreeSet<Who>>,
    /// Everyone else's trading since the oldest open run started.
    others: Vec<Traded>,
    /// The slot the current step's takes are made at.
    step: u64,
    /// Where and by whom the bait was set.
    set: Option<(u64, Who)>,
}

impl BaitWatch {
    /// A watch for the trader whose wallets are `own`, its runs `horizon`
    /// slots long, reading the other wallets' takes from `ledger`.
    pub fn new(spec: Bait, horizon: u64, own: HashSet<Pubkey>, ledger: Ledger) -> Self {
        Self {
            spec,
            horizon: horizon.max(1),
            own,
            ledger,
            read: 0,
            open: VecDeque::new(),
            latest: None,
            judged: VecDeque::new(),
            others: Vec::new(),
            step: 0,
            set: None,
        }
    }

    /// The start of a step at `at`: read who traded since the last, judge
    /// the runs that are due, and set the bait if one account has matched
    /// enough of them. Who set it, if it was set now.
    pub fn begin(&mut self, at: u64) -> Option<Who> {
        self.step = at;
        let takes = self.ledger.takes_from(self.read);
        self.read += takes.len();
        for (at, wallet, side, size) in takes {
            if !self.own.contains(&wallet) {
                self.others.push(Traded {
                    at,
                    who: Who::Wallet(wallet),
                    side,
                    size,
                });
            }
        }
        if self.set.is_some() {
            self.open.clear();
            self.others.clear();
            return None;
        }
        while let Some(run) = self.open.front() {
            let window_over = at > run.start + self.spec.follow_slots;
            let measured = run.fills.len() >= self.spec.ref_fills as usize;
            let run_over = at >= run.start + self.horizon;
            if !(window_over && (measured || run_over)) {
                break;
            }
            let run = self.open.pop_front().expect("checked");
            let matched = self.matched(&run);
            self.judged.push_back(matched);
            while self.judged.len() > self.spec.episodes as usize {
                self.judged.pop_front();
            }
            if let Some(who) = self.copier() {
                self.set = Some((at, who));
                self.open.clear();
                self.others.clear();
                return Some(who);
            }
        }
        self.prune();
        None
    }

    /// The bot's trading over the step, from its position. It landed after
    /// the step began.
    pub fn bot(&mut self, flow: &StepFlow) {
        if self.set.is_some() {
            return;
        }
        for side in [Side::Bid, Side::Ask] {
            let size = flow.bot_traded(side);
            if size > 0.0 {
                self.others.push(Traded {
                    at: self.step + 1,
                    who: Who::Bot,
                    side,
                    size,
                });
            }
        }
    }

    /// One of its own takes landed: `size` on `side` at `at`.
    pub fn filled(&mut self, at: u64, side: Side, size: f64) {
        if self.set.is_some() {
            return;
        }
        let fresh = self
            .latest
            .as_ref()
            .is_none_or(|run| run.side != side || at >= run.start + self.horizon);
        if fresh {
            let run = Run {
                start: at,
                side,
                fills: vec![size],
            };
            self.latest = Some(run.clone());
            self.open.push_back(run);
            return;
        }
        let latest = self.latest.as_mut().expect("checked");
        if latest.fills.len() < self.spec.ref_fills as usize {
            latest.fills.push(size);
            if let Some(open) = self.open.back_mut()
                && open.start == latest.start
            {
                open.fills = latest.fills.clone();
            }
        }
    }

    /// Whether the bait is set.
    pub fn is_set(&self) -> bool {
        self.set.is_some()
    }

    /// Forget trading too old to fall in any open run's window.
    fn prune(&mut self) {
        let from = self.open.front().map_or(self.step, |run| run.start);
        self.others.retain(|t| t.at > from);
    }

    /// The accounts whose net flow on `run`'s side, within its window, came
    /// to `match_share` of its first takes.
    fn matched(&self, run: &Run) -> BTreeSet<Who> {
        let own: f64 = run.fills.iter().sum();
        let end = run.start + self.spec.follow_slots;
        let mut net = BTreeMap::<Who, f64>::new();
        for traded in &self.others {
            if traded.at > run.start && traded.at <= end {
                let way = if traded.side == run.side { 1.0 } else { -1.0 };
                *net.entry(traded.who).or_default() += way * traded.size;
            }
        }
        net.into_iter()
            .filter(|(_, size)| *size > 0.0 && *size >= self.spec.match_share * own)
            .map(|(who, _)| who)
            .collect()
    }

    /// The account that matched `matches` of the last judged runs, if one
    /// did (the bot first).
    fn copier(&self) -> Option<Who> {
        let mut counts = BTreeMap::<Who, u32>::new();
        for matched in &self.judged {
            for who in matched {
                *counts.entry(*who).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .rev()
            .find(|(_, n)| *n >= self.spec.matches)
            .map(|(who, _)| who)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{VenueFlow, library::Ledger};

    fn spec() -> Bait {
        Bait {
            follow_slots: 10,
            match_share: 0.6,
            matches: 3,
            episodes: 8,
            ref_fills: 3,
            hit_rate_after: 0.5,
        }
    }

    fn bought(size: f64) -> StepFlow {
        StepFlow {
            bot: vec![VenueFlow {
                market: "test".into(),
                bought: size,
                sold: 0.0,
            }],
        }
    }

    /// Runs of three fills of 1 every 100 slots from slot 100, buying; the
    /// bot buys `copy` 4 slots after the start of each run in `copied`.
    fn followed(copy: f64, copied: &[u64], until: u64) -> (BaitWatch, Option<(u64, Who)>) {
        let mut watch = BaitWatch::new(spec(), 60, HashSet::new(), Ledger::with_takes());
        let mut set = None;
        for at in (2..=until).step_by(2) {
            if let Some(who) = watch.begin(at) {
                set = Some((at, who));
            }
            let start = at / 100 * 100;
            if at % 100 == 4 && copied.contains(&start) {
                watch.bot(&bought(copy));
            }
            if at >= 100 && [0, 2, 4].contains(&(at % 100)) {
                watch.filled(at, Side::Bid, 1.0);
            }
        }
        (watch, set)
    }

    #[test]
    fn a_copier_of_enough_runs_sets_the_bait() {
        let (watch, set) = followed(2.0, &[100, 200, 300, 400], 1_000);
        assert!(watch.is_set());
        // The third copied run is judged once its window is over.
        let (at, who) = set.unwrap();
        assert_eq!(who, Who::Bot);
        assert!((311..=320).contains(&at), "{at}");
    }

    #[test]
    fn copying_under_the_share_or_too_few_runs_does_not() {
        // 1.7 is under 0.6 of the run's 3.
        let (watch, _) = followed(1.7, &[100, 200, 300, 400, 500], 1_000);
        assert!(!watch.is_set());
        // Two runs in every eight.
        let (watch, _) = followed(5.0, &[100, 200, 900, 1_000, 1_700, 1_800], 2_000);
        assert!(!watch.is_set());
        // Three, but spread over more than eight runs.
        let (watch, _) = followed(5.0, &[100, 600, 1_100, 1_600], 2_000);
        assert!(!watch.is_set());
    }

    #[test]
    fn only_net_flow_its_way_within_the_window_from_one_account_counts() {
        let ledger = Ledger::with_takes();
        let (a, b, own) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut watch = BaitWatch::new(spec(), 60, HashSet::from([own]), ledger.clone());
        let mut set = None;
        for at in (2..=600).step_by(2) {
            let start = at / 100 * 100;
            if at >= 100 && at % 100 == 6 {
                // `a` buys 2 after each run's start but sells 1.5 too: net
                // 0.5, under the share. `b` buys 1 twice: 2, at the share,
                // but once at the run's own slot and once past the window,
                // which don't count. Its own wallet never counts.
                ledger.took(start + 3, a, Side::Bid, 2.0);
                ledger.took(start + 5, a, Side::Ask, 1.5);
                ledger.took(start, b, Side::Bid, 1.0);
                ledger.took(start + 11, b, Side::Bid, 1.0);
                ledger.took(start + 3, own, Side::Bid, 9.0);
            }
            if let Some(who) = watch.begin(at) {
                set = Some(who);
            }
            if at >= 100 && [0, 2, 4].contains(&(at % 100)) {
                watch.filled(at, Side::Bid, 1.0);
            }
        }
        assert_eq!(set, None);
        // Once `b`'s buys fall inside the window, it matches every run.
        for at in (602..=1_000).step_by(2) {
            let start = at / 100 * 100;
            if at % 100 == 6 {
                ledger.took(start + 1, b, Side::Bid, 1.0);
                ledger.took(start + 4, b, Side::Bid, 1.0);
            }
            if let Some(who) = watch.begin(at) {
                set = Some(who);
            }
            if [0, 2, 4].contains(&(at % 100)) {
                watch.filled(at, Side::Bid, 1.0);
            }
        }
        assert_eq!(set, Some(Who::Wallet(b)));
    }

    #[test]
    fn a_run_starts_on_the_other_side_or_after_a_horizon() {
        let mut watch = BaitWatch::new(spec(), 60, HashSet::new(), Ledger::with_takes());
        watch.begin(10);
        watch.filled(10, Side::Bid, 1.0);
        watch.filled(30, Side::Bid, 1.0);
        watch.filled(40, Side::Ask, 1.0);
        watch.filled(60, Side::Ask, 1.0);
        watch.filled(100, Side::Ask, 1.0);
        let starts: Vec<(u64, usize)> = watch.open.iter().map(|r| (r.start, r.fills.len())).collect();
        assert_eq!(starts, [(10, 2), (40, 2), (100, 1)]);
    }

    #[test]
    fn a_bait_setting_is_checked() {
        assert!(spec().validate().is_ok());
        for bad in [
            Bait {
                follow_slots: 0,
                ..spec()
            },
            Bait {
                matches: 0,
                ..spec()
            },
            Bait {
                matches: 9,
                ..spec()
            },
            Bait {
                episodes: 0,
                ..spec()
            },
            Bait {
                ref_fills: 0,
                ..spec()
            },
            Bait {
                match_share: f64::NAN,
                ..spec()
            },
            Bait {
                hit_rate_after: 0.4,
                ..spec()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
