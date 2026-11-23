//! Cycles through a session's pools that start and end in SOL, sized on the
//! pools' exact quotes: what a cycle searcher sends, and what a session's
//! report measures a trigger's worth by.
//!
//! A session's pools are a graph whose nodes are tokens and whose edges are
//! one direction of one pool. [`Graph::new`] lists every simple cycle
//! through SOL of at most [`LEGS_MAX`] legs once, as the session starts: a
//! pool appears at most once in a cycle, and no token but SOL twice. On the
//! pools as they are, [`Graph::best`] skips every cycle whose marginal rate
//! is at most 1 (the product of each leg's price after its fee: no size of
//! it can profit, since every leg's price moves against it as it trades),
//! and sizes the rest by golden-section search on the chained exact quotes,
//! as the research's detector does (`ops/research/atomic-arb/lst-cycles/
//! detector.py`, there after a Bellman–Ford gate over the same rates). Two
//! SOL/USDC pools hold two cycles, one each way, and the one the gate lets
//! through is the sale into the dearer pool.

use solana_pubkey::Pubkey;

use crate::venue::amm::{Direction, Mode, Pool, Quoted, State};

/// The most legs a cycle has: the research's detector enumerated cycles of
/// up to 4, and its 5-leg routes were a handful in 400.
pub const LEGS_MAX: usize = 4;
/// The most cycles a graph lists: the snapshot of ten pools over five
/// tokens holds a few dozen of at most four legs.
const CYCLES_MAX: usize = 4_096;
/// Golden-section steps the search takes: each narrows the range by 0.618,
/// so 60 take a range of 10^4 SOL to under an atom.
const SEARCH_STEPS: u32 = 60;
/// The least a cycle puts in, in atoms of SOL: 0.001 SOL.
const SOL_IN_MIN_ATOMS: u64 = 1_000_000;

/// One leg of a cycle: a pool and the way through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    pub pool: usize,
    pub direction: Direction,
}

/// A cycle quoted on the pools' states: leg `i` puts in exactly what leg
/// `i - 1` was quoted to give, the first the SOL put in.
#[derive(Debug, Clone)]
pub struct Cycle {
    pub legs: Vec<Leg>,
    pub quotes: Vec<Quoted>,
}

impl Cycle {
    /// The SOL it ends with less the SOL it put in, in atoms.
    pub fn profit_atoms(&self) -> i128 {
        let first = self.quotes.first().expect("a cycle has legs");
        let last = self.quotes.last().expect("a cycle has legs");
        i128::from(last.amount_out) - i128::from(first.amount_in)
    }

    pub fn sol_in_atoms(&self) -> u64 {
        self.quotes.first().expect("a cycle has legs").amount_in
    }
}

/// Every cycle through SOL of a session's pools.
#[derive(Debug, Clone)]
pub struct Graph {
    cycles: Vec<Vec<Leg>>,
}

impl Graph {
    /// The cycles through `sol` of at most [`LEGS_MAX`] legs, two at least.
    pub fn new(pools: &[Pool], sol: &Pubkey) -> Self {
        let mut cycles = Vec::new();
        let mut path = Vec::with_capacity(LEGS_MAX);
        extend(pools, sol, sol, &mut path, &mut cycles);
        assert!(cycles.len() <= CYCLES_MAX, "a graph of a session's pools");
        assert!(
            cycles
                .iter()
                .all(|c| (2..=LEGS_MAX).contains(&c.len()) && ends(pools, c) == *sol)
        );
        Self { cycles }
    }

    /// How many cycles it holds of at most `max_legs` legs.
    pub fn count(&self, max_legs: usize) -> usize {
        self.cycles.iter().filter(|c| c.len() <= max_legs).count()
    }

    /// The cycle of at most `max_legs` legs that profits most on `states`,
    /// putting in `fixed` atoms of SOL or the best size up to `max_in`.
    /// `None` when none profits.
    pub fn best(
        &self,
        pools: &[Pool],
        states: &[State],
        max_legs: usize,
        max_in: u64,
        fixed: Option<u64>,
    ) -> Option<Cycle> {
        assert_eq!(pools.len(), states.len(), "a state per pool");
        assert!((2..=LEGS_MAX).contains(&max_legs));
        let mut best: Option<Cycle> = None;
        for legs in self.cycles.iter().filter(|c| c.len() <= max_legs) {
            if marginal_rate(pools, states, legs) <= 1.0 {
                continue;
            }
            let Some(cycle) = sized(pools, states, legs, max_in, fixed) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|b| cycle.profit_atoms() > b.profit_atoms())
            {
                best = Some(cycle);
            }
        }
        assert!(best.as_ref().is_none_or(|c| c.profit_atoms() > 0));
        best
    }
}

/// The token a leg pays in, and the one it receives.
fn tokens(pool: &Pool, direction: Direction) -> (Pubkey, Pubkey) {
    match direction {
        Direction::BaseIn => (pool.base.mint, pool.quote.mint),
        Direction::QuoteIn => (pool.quote.mint, pool.base.mint),
    }
}

/// The token `legs` end in.
fn ends(pools: &[Pool], legs: &[Leg]) -> Pubkey {
    let last = legs.last().expect("a cycle has legs");
    tokens(&pools[last.pool], last.direction).1
}

/// Depth-first: every way on from `at` that uses a pool not yet in `path`
/// and reaches a token not yet visited, or closes on `sol`. Bounded by
/// [`LEGS_MAX`].
fn extend(pools: &[Pool], sol: &Pubkey, at: &Pubkey, path: &mut Vec<Leg>, out: &mut Vec<Vec<Leg>>) {
    if path.len() == LEGS_MAX {
        return;
    }
    for (i, pool) in pools.iter().enumerate() {
        if path.iter().any(|leg| leg.pool == i) {
            continue;
        }
        for direction in [Direction::BaseIn, Direction::QuoteIn] {
            let (pays, receives) = tokens(pool, direction);
            if pays != *at {
                continue;
            }
            let leg = Leg { pool: i, direction };
            if receives == *sol {
                // A cycle of one pool there and back isn't one.
                if !path.is_empty() {
                    let mut cycle = path.clone();
                    cycle.push(leg);
                    out.push(cycle);
                }
                continue;
            }
            let visited = path
                .iter()
                .any(|l| tokens(&pools[l.pool], l.direction).1 == receives);
            if visited {
                continue;
            }
            path.push(leg);
            extend(pools, sol, &receives, path, out);
            path.pop();
        }
    }
}

/// The product of each leg's price after its fee, in whole units: what a
/// unit of SOL becomes through the cycle at its first atom.
fn marginal_rate(pools: &[Pool], states: &[State], legs: &[Leg]) -> f64 {
    legs.iter().fold(1.0, |rate, leg| {
        let (pool, state) = (&pools[leg.pool], &states[leg.pool]);
        let price = match leg.direction {
            Direction::BaseIn => state.mid(pool),
            Direction::QuoteIn => 1.0 / state.mid(pool),
        };
        rate * price * (1.0 - state.fee_fraction())
    })
}

/// `legs` quoted for `sol_in` atoms, each leg exact in for what the one
/// before it gives. `None` when a pool can't take it.
fn quoted(pools: &[Pool], states: &[State], legs: &[Leg], sol_in: u64) -> Option<Cycle> {
    let mut quotes: Vec<Quoted> = Vec::with_capacity(legs.len());
    let mut amount = sol_in;
    for leg in legs {
        let quote = states[leg.pool]
            .quote(
                &pools[leg.pool].address,
                leg.direction,
                Mode::ExactIn,
                amount,
            )
            .ok()?;
        amount = quote.amount_out;
        quotes.push(quote);
    }
    Some(Cycle {
        legs: legs.to_vec(),
        quotes,
    })
}

/// `legs` at `fixed` atoms, or at the size that profits most of at most
/// `max_in`: profit is concave in the size, as each pool's price moves
/// against the cycle while it trades. `None` when it doesn't profit.
fn sized(
    pools: &[Pool],
    states: &[State],
    legs: &[Leg],
    max_in: u64,
    fixed: Option<u64>,
) -> Option<Cycle> {
    let profit =
        |sol_in: u64| quoted(pools, states, legs, sol_in).map_or(i128::MIN, |c| c.profit_atoms());
    let chosen = match fixed {
        Some(sol_in) => sol_in,
        None => {
            if max_in <= SOL_IN_MIN_ATOMS {
                return None;
            }
            let ratio = (5f64.sqrt() - 1.0) / 2.0;
            let (mut lo, mut hi) = (SOL_IN_MIN_ATOMS as f64, max_in as f64);
            for _ in 0..SEARCH_STEPS {
                let a = hi - ratio * (hi - lo);
                let b = lo + ratio * (hi - lo);
                if profit(a as u64) >= profit(b as u64) {
                    hi = b;
                } else {
                    lo = a;
                }
            }
            ((lo + hi) / 2.0) as u64
        }
    };
    let cycle = quoted(pools, states, legs, chosen)?;
    (cycle.profit_atoms() > 0).then_some(cycle)
}

/// The two pools' gap now, in basis points: how far the dearer one prices
/// SOL above the cheaper. A session of two pools reports it.
pub fn gap_bps(pools: &[Pool], states: &[State]) -> f64 {
    assert_eq!(pools.len(), states.len(), "a state per pool");
    assert_eq!(pools.len(), 2, "a gap is between two pools");
    let mids = [states[0].mid(&pools[0]), states[1].mid(&pools[1])];
    (mids[0].max(mids[1]) / mids[0].min(mids[1]) - 1.0) * 10_000.0
}

#[cfg(test)]
mod tests {
    use mtx_core::Asset;

    use super::*;
    use crate::venue::amm::{Program, Token};

    fn token(mint: Pubkey) -> Token {
        Token {
            mint,
            token_program: Pubkey::new_unique(),
            vault: Pubkey::new_unique(),
            asset: Asset::new(mint.to_string(), mint.to_string(), 9).unwrap(),
        }
    }

    fn pool(base: Pubkey, quote: Pubkey) -> Pool {
        let address = Pubkey::new_unique();
        Pool {
            instrument: format!("orca:amm:{address}").parse().unwrap(),
            address,
            program: Program::Whirlpool,
            base: token(base),
            quote: token(quote),
        }
    }

    #[test]
    fn two_pools_of_a_pair_hold_a_cycle_each_way() {
        let (sol, usdc) = (Pubkey::new_unique(), Pubkey::new_unique());
        let pools = [pool(sol, usdc), pool(sol, usdc)];
        let graph = Graph::new(&pools, &sol);
        assert_eq!(graph.count(LEGS_MAX), 2);
        assert_eq!(graph.count(2), 2);
    }

    /// The snapshot's graph: SOL/USDC twice, SOL/JitoSOL, JTO/JitoSOL
    /// twice, SOL/JTO twice, SOL/GEOD twice and GEOD/USDC.
    #[test]
    fn the_thin_pools_graph_holds_its_two_three_and_four_leg_cycles() {
        let [sol, usdc, jitosol, jto, geod] = [(); 5].map(|_| Pubkey::new_unique());
        let pools = [
            pool(sol, usdc),
            pool(sol, usdc),
            pool(sol, jitosol),
            pool(jto, jitosol),
            pool(jto, jitosol),
            pool(sol, jto),
            pool(sol, jto),
            pool(sol, geod),
            pool(sol, geod),
            pool(geod, usdc),
        ];
        let graph = Graph::new(&pools, &sol);
        // Two legs: a pair's two pools, each way, for SOL/USDC, SOL/JTO
        // and SOL/GEOD.
        assert_eq!(graph.count(2), 6);
        // Three legs: SOL, JitoSOL, JTO (2 × 2 pools, each way) and SOL,
        // GEOD, USDC (2 × 2, each way).
        assert_eq!(graph.count(3) - graph.count(2), 16);
        // Four legs would need three tokens beside SOL in a chain, and the
        // graph's only links beside SOL are JitoSOL–JTO and GEOD–USDC.
        assert_eq!(graph.count(4), graph.count(3));
        for cycle in &graph.cycles {
            let mut pools_used: Vec<usize> = cycle.iter().map(|l| l.pool).collect();
            pools_used.sort_unstable();
            pools_used.dedup();
            assert_eq!(pools_used.len(), cycle.len(), "a pool at most once");
            assert_eq!(tokens(&pools[cycle[0].pool], cycle[0].direction).0, sol);
        }
    }
}
