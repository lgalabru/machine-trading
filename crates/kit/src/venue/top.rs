//! The top of a Phoenix perp book, found without building the book.
//!
//! The book is the resting orders and every enabled spline's ladder, then
//! uncrossed: a spline's bid can sit at or above another's ask until they
//! match. Building it level by level puts thousands of ticks in an ordered
//! map on every read (a spline side walks up to 4,096), and only the best
//! level of each side, after the uncrossing, is wanted.
//!
//! Each spline side's ladder comes out already sorted, best first, as the
//! walk moves away from its mid. So the sides are merges of sorted streams,
//! read lazily: the uncrossing pulls levels only as deep as the book
//! crosses, and the top is the two levels left at the head. Equal prices
//! across streams are one level, as in the built book.

use std::collections::BinaryHeap;

/// A spline region as the walk reads it: active, with something in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Region {
    pub start: u64,
    pub end: u64,
    pub lots_per_tick: u64,
    pub filled: u64,
}

/// One side of one spline's ladder, walked a tick at a time: the levels
/// Phoenix's `liquidity_at_slot_with_capacity` lays out, best first.
#[derive(Debug, Clone)]
pub(crate) struct Walk {
    regions: Vec<Region>,
    mid: u64,
    base_width: u64,
    is_bid: bool,
    lots_left: u64,
    ticks_left: u64,
    region: usize,
    tick: u64,
    fills_left: u64,
    started: bool,
}

impl Walk {
    /// `regions` in the order the spline holds them, `lots_max` the side's
    /// capacity, `ticks_max` the most ticks walked.
    pub fn new(
        regions: Vec<Region>,
        mid: u64,
        base_width: u64,
        is_bid: bool,
        lots_max: u64,
        ticks_max: u64,
    ) -> Self {
        Self {
            regions,
            mid,
            base_width,
            is_bid,
            lots_left: lots_max,
            ticks_left: ticks_max,
            region: 0,
            tick: 0,
            fills_left: 0,
            started: false,
        }
    }

    /// Whether its levels come out sorted, best first: regions in order of
    /// their offsets, none overlapping. A spline whose regions aren't is
    /// read whole instead.
    pub fn regions_in_order(&self) -> bool {
        self.regions
            .windows(2)
            .all(|pair| pair[0].end <= pair[1].start)
    }
}

impl Iterator for Walk {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        loop {
            let region = *self.regions.get(self.region)?;
            if !self.started {
                self.tick = region.start;
                self.fills_left = region.filled;
                self.started = true;
            }
            if !(self.tick < region.end && self.lots_left > 0 && self.ticks_left > 0) {
                self.region += 1;
                self.started = false;
                continue;
            }
            let filled = self.fills_left.min(region.lots_per_tick);
            self.fills_left -= filled;
            let lots = (region.lots_per_tick - filled).min(self.lots_left);
            let offset = self.tick.saturating_add(self.base_width);
            self.tick += 1;
            self.ticks_left -= 1;
            if lots == 0 {
                continue;
            }
            let ticks = match self.is_bid {
                true => self.mid.checked_sub(offset),
                false => self.mid.checked_add(offset),
            };
            if let Some(ticks) = ticks.filter(|&ticks| ticks > 0) {
                self.lots_left -= lots;
                return Some((ticks, lots));
            }
        }
    }
}

/// A side's levels, best first, merged from sorted streams.
struct Side {
    is_bid: bool,
    streams: Vec<Box<dyn Iterator<Item = (u64, u64)>>>,
    /// Each stream's head, keyed so the best is the heap's greatest.
    heads: BinaryHeap<(u64, usize, u64)>,
}

impl Side {
    fn new(is_bid: bool, streams: Vec<Box<dyn Iterator<Item = (u64, u64)>>>) -> Self {
        let mut side = Self {
            is_bid,
            heads: BinaryHeap::with_capacity(streams.len()),
            streams,
        };
        for index in 0..side.streams.len() {
            side.advance(index);
        }
        side
    }

    fn key(&self, ticks: u64) -> u64 {
        match self.is_bid {
            true => ticks,
            false => u64::MAX - ticks,
        }
    }

    fn advance(&mut self, index: usize) {
        if let Some((ticks, lots)) = self.streams[index].next() {
            debug_assert!(lots > 0, "a level holds lots");
            let key = self.key(ticks);
            self.heads.push((key, index, lots));
        }
    }

    /// The next level: every stream's lots at the best price left.
    fn level(&mut self) -> Option<(u64, u64)> {
        let (key, index, mut lots) = self.heads.pop()?;
        self.advance(index);
        while let Some(&(next, index, more)) = self.heads.peek()
            && next == key
        {
            self.heads.pop();
            lots = lots.saturating_add(more);
            self.advance(index);
        }
        let ticks = match self.is_bid {
            true => key,
            false => u64::MAX - key,
        };
        Some((ticks, lots))
    }
}

/// The best bid and ask, in ticks, of the book made of `bids` and `asks`
/// (each a list of sorted streams, best first), once uncrossed: the best
/// bid matched against the best ask while they lock or cross.
pub(crate) fn top(
    bids: Vec<Box<dyn Iterator<Item = (u64, u64)>>>,
    asks: Vec<Box<dyn Iterator<Item = (u64, u64)>>>,
) -> (Option<u64>, Option<u64>) {
    let (mut bids, mut asks) = (Side::new(true, bids), Side::new(false, asks));
    let (mut bid, mut ask) = (bids.level(), asks.level());
    // Each round empties a level; the streams are finite (a spline side
    // walks a bounded number of ticks), so this ends.
    while let (Some((bid_ticks, bid_lots)), Some((ask_ticks, ask_lots))) = (bid, ask) {
        if bid_ticks < ask_ticks {
            break;
        }
        let matched = bid_lots.min(ask_lots);
        bid = match bid_lots - matched {
            0 => bids.level(),
            left => Some((bid_ticks, left)),
        };
        ask = match ask_lots - matched {
            0 => asks.level(),
            left => Some((ask_ticks, left)),
        };
    }
    let top = (bid.map(|(ticks, _)| ticks), ask.map(|(ticks, _)| ticks));
    if let (Some(bid), Some(ask)) = top {
        assert!(bid < ask, "an uncrossed book: bid {bid} under ask {ask}");
    }
    top
}

/// Orders at prices in any order, as one sorted stream, best first, equal
/// prices one level.
pub(crate) fn sorted(
    mut levels: Vec<(u64, u64)>,
    is_bid: bool,
) -> Box<dyn Iterator<Item = (u64, u64)>> {
    levels.retain(|&(_, lots)| lots > 0);
    match is_bid {
        true => levels.sort_unstable_by_key(|level| std::cmp::Reverse(level.0)),
        false => levels.sort_unstable_by_key(|level| level.0),
    }
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(levels.len());
    for (ticks, lots) in levels {
        match merged.last_mut() {
            Some(last) if last.0 == ticks => last.1 = last.1.saturating_add(lots),
            _ => merged.push((ticks, lots)),
        }
    }
    Box::new(merged.into_iter())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// The book built whole and uncrossed, as the venue built it before:
    /// what the merge must agree with.
    fn built(bids: Vec<Vec<(u64, u64)>>, asks: Vec<Vec<(u64, u64)>>) -> (Option<u64>, Option<u64>) {
        let ladder = |streams: Vec<Vec<(u64, u64)>>| {
            let mut side = BTreeMap::new();
            for (ticks, lots) in streams.into_iter().flatten() {
                *side.entry(ticks).or_insert(0u64) += lots;
            }
            side
        };
        let (mut bids, mut asks) = (ladder(bids), ladder(asks));
        while let (Some((&bid, &bid_lots)), Some((&ask, &ask_lots))) =
            (bids.last_key_value(), asks.first_key_value())
        {
            if bid < ask {
                break;
            }
            let matched = bid_lots.min(ask_lots);
            for (side, ticks, lots) in [(&mut bids, bid, bid_lots), (&mut asks, ask, ask_lots)] {
                if lots == matched {
                    side.remove(&ticks);
                } else {
                    *side.get_mut(&ticks).unwrap() -= matched;
                }
            }
        }
        (
            bids.last_key_value().map(|(&t, _)| t),
            asks.first_key_value().map(|(&t, _)| t),
        )
    }

    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % sides
        }
    }

    fn walk(dice: &mut Dice, mid: u64, is_bid: bool) -> Walk {
        let mut regions = Vec::new();
        let mut at = dice.roll(4);
        for _ in 0..dice.roll(4) {
            let start = at + dice.roll(3);
            let end = start + 1 + dice.roll(30);
            regions.push(Region {
                start,
                end,
                lots_per_tick: 1 + dice.roll(20),
                filled: dice.roll(40),
            });
            at = end;
        }
        Walk::new(
            regions,
            mid,
            dice.roll(5),
            is_bid,
            1 + dice.roll(400),
            1 + dice.roll(200),
        )
    }

    /// Thousands of random books, spline mids on either side of each
    /// other so they cross, with resting orders: the merge's top is the
    /// built book's.
    #[test]
    fn the_merge_finds_the_built_book_s_top() {
        let mut crossed = 0;
        for seed in 1..=3_000u64 {
            let mut dice = Dice(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut bid_streams = Vec::new();
            let mut ask_streams = Vec::new();
            for _ in 0..1 + dice.roll(5) {
                let mid = 1_000 + dice.roll(60);
                bid_streams.push(walk(&mut dice, mid, true).collect::<Vec<_>>());
                ask_streams.push(walk(&mut dice, mid, false).collect::<Vec<_>>());
            }
            let orders = |dice: &mut Dice| {
                (0..dice.roll(8))
                    .map(|_| (980 + dice.roll(80), 1 + dice.roll(50)))
                    .collect::<Vec<_>>()
            };
            let (bid_orders, ask_orders) = (orders(&mut dice), orders(&mut dice));
            for stream in bid_streams.iter().chain(&ask_streams) {
                assert!(
                    stream.windows(2).all(|w| w[0].0 != w[1].0),
                    "a walk visits a price once"
                );
            }
            let streams = |walks: &[Vec<(u64, u64)>], orders: &[(u64, u64)], is_bid: bool| {
                let mut out: Vec<Box<dyn Iterator<Item = (u64, u64)>>> =
                    vec![sorted(orders.to_vec(), is_bid)];
                out.extend(walks.iter().map(|w| {
                    Box::new(w.clone().into_iter()) as Box<dyn Iterator<Item = (u64, u64)>>
                }));
                out
            };
            let merged = top(
                streams(&bid_streams, &bid_orders, true),
                streams(&ask_streams, &ask_orders, false),
            );
            let mut all_bids = bid_streams.clone();
            all_bids.push(bid_orders.clone());
            let mut all_asks = ask_streams.clone();
            all_asks.push(ask_orders.clone());
            assert_eq!(
                merged,
                built(all_bids.clone(), all_asks.clone()),
                "seed {seed}"
            );
            let max_bid = all_bids.iter().flatten().map(|l| l.0).max();
            let min_ask = all_asks.iter().flatten().map(|l| l.0).min();
            if let (Some(b), Some(a)) = (max_bid, min_ask)
                && b >= a
            {
                crossed += 1;
            }
        }
        assert!(
            crossed > 300,
            "the books crossed often enough to test the uncrossing: {crossed}"
        );
    }

    #[test]
    fn a_walk_comes_out_best_first() {
        let regions = vec![
            Region {
                start: 0,
                end: 3,
                lots_per_tick: 5,
                filled: 7,
            },
            Region {
                start: 3,
                end: 5,
                lots_per_tick: 2,
                filled: 0,
            },
        ];
        let bids: Vec<_> = Walk::new(regions.clone(), 100, 1, true, 100, 100).collect();
        // The first region's fills take its first tick's 5 lots and 2 of
        // the next's; a tick's offset is its own plus the base width.
        assert_eq!(bids, [(98, 3), (97, 5), (96, 2), (95, 2)]);
        let asks: Vec<_> = Walk::new(regions, 100, 1, false, 6, 100).collect();
        assert_eq!(asks, [(102, 3), (103, 3)], "capacity stops the walk");
        let unordered = vec![
            Region {
                start: 5,
                end: 9,
                lots_per_tick: 1,
                filled: 0,
            },
            Region {
                start: 0,
                end: 4,
                lots_per_tick: 1,
                filled: 0,
            },
        ];
        assert!(!Walk::new(unordered, 100, 0, true, 9, 9).regions_in_order());
    }
}
