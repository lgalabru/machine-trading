//! Pools the runtime watches without trading them (`control.pools`): what a
//! sidecar looking for arbitrage across pools needs to see of each, as the
//! chain changes it.
//!
//! Each pool's account joins the feed's watch list and the account cache,
//! with the accounts a quote through it needs: a Whirlpool's tick arrays
//! and oracle, a Raydium pool's config, bitmap extension and tick arrays.
//! After the strategies' steps, every pool a write changed since it was
//! last read (and every pool once in [`PoolsConfig::every_slots`]) is
//! assembled from the cache with the venue's own code
//! ([`amm::State::assemble`]), quoted with the kit's exact quotes, and told
//! on the event stream as a `pool`: its price, and a ladder of swaps each
//! way ([`reading`]).
//!
//! A pool's tick arrays change as its price walks: the ones a new price
//! needs are watched and read once from the node by a task of their own,
//! and the pool is told again once they are in: the strategies' loop never
//! waits on the node. The quotes use this host's clock where the programs
//! use the chain's, which only a fee that decays by the second can tell
//! apart.

use std::time::Instant;

use mtx_core::{
    events::{Event, PoolRung},
    mandate::InstrumentRef,
};
use serde::Deserialize;
use solana_pubkey::Pubkey;

use super::feed::{Accounts, Snapshot};
use crate::venue::amm::{self, Assembly, Lookup};

/// Pools watched at most: each written pool is read and quoted in its
/// slot, about a millisecond each, after the strategies' steps.
pub const POOLS_MAX: usize = 64;

/// Rungs each way at most: each is an exact quote, tens of microseconds.
pub const RUNGS_MAX: usize = 16;

/// The furthest a rung moves a pool's price, in bps: past a third of the
/// price a swap is no arbitrage's leg.
pub const RUNG_BPS_MAX: u32 = 3_000;

/// Accounts one pool may have watched: its own, its fixed ones, and the
/// tick arrays its price walked through since the runtime started (a
/// Raydium pool reads up to 32 at once). Past it the pool is no longer
/// read, and says so: the watch list has no way to forget.
pub const POOL_ACCOUNTS_MAX: usize = 512;

/// The fewest slots between two reads of a pool nobody wrote: reading it
/// every slot would tell the same ladder 150 times a minute.
pub const EVERY_SLOTS_MIN: u64 = 25;

/// A minute: a pool nobody traded still tells a sidecar it is there.
fn default_every_slots() -> u64 {
    150
}

/// From a pool's fee band to well past what a large swap opens: on
/// 2026-11-21 a 500 SOL chunk moved Raydium's SOL/USDC pool about 30 bp,
/// and thin pools' triggers had a median of 545 bp.
fn default_rungs_bps() -> Vec<u32> {
    vec![2, 5, 10, 20, 40, 80, 160, 320, 640]
}

/// What to watch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolsConfig {
    /// The pools, as mandates name them: `orca:amm:<address>` and
    /// `raydium:amm:<address>`.
    pub pools: Vec<InstrumentRef>,
    /// A pool nobody wrote is read again after this many slots.
    #[serde(default = "default_every_slots")]
    pub every_slots: u64,
    /// The ladder: how far each rung moves the pool's price, in bps,
    /// growing.
    #[serde(default = "default_rungs_bps")]
    pub rungs_bps: Vec<u32>,
}

impl PoolsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.pools.len() > POOLS_MAX {
            return Err(format!("{} pools: {POOLS_MAX} at most", self.pools.len()));
        }
        let rungs = &self.rungs_bps;
        if rungs.is_empty() || rungs.len() > RUNGS_MAX {
            return Err(format!("{} rungs: 1 to {RUNGS_MAX}", rungs.len()));
        }
        let grows = rungs.windows(2).all(|pair| pair[0] < pair[1]);
        let in_range = rungs.iter().all(|bps| (1..=RUNG_BPS_MAX).contains(bps));
        if !(grows && in_range) {
            return Err(format!("rungs_bps {rungs:?}: growing, 1 to {RUNG_BPS_MAX}"));
        }
        Ok(())
    }
}

/// One pool as it stood: what a `pool` event carries beside its slots.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub mid: f64,
    pub fee_bps: f64,
    pub liquidity: f64,
    pub sells: Vec<PoolRung>,
    pub buys: Vec<PoolRung>,
}

/// `state`'s price and its ladder each way: for each of `rungs_bps`, the
/// exact swap that moves the price that far. A side's ladder ends at the
/// first rung the pool can't fill (the curve's end, or more tick arrays
/// than a swap may enter): what is told is what would land.
pub fn reading(pool: &amm::Pool, state: &amm::State, rungs_bps: &[u32]) -> Reading {
    assert!(rungs_bps.len() <= RUNGS_MAX, "the rungs are bounded");
    let mid = state.mid(pool);
    let side = |direction: amm::Direction| {
        let (input, output) = match direction {
            amm::Direction::BaseIn => (&pool.base, &pool.quote),
            amm::Direction::QuoteIn => (&pool.quote, &pool.base),
        };
        let units = |atoms: u64, token: &amm::Token| {
            atoms as f64 / 10f64.powi(i32::from(token.asset.decimals))
        };
        let mut rungs: Vec<PoolRung> = Vec::with_capacity(rungs_bps.len());
        for bps in rungs_bps {
            let moved = f64::from(*bps) / 10_000.0;
            let target = match direction {
                amm::Direction::BaseIn => mid * (1.0 - moved),
                amm::Direction::QuoteIn => mid * (1.0 + moved),
            };
            // No cap: the price limit ends the swap.
            let Ok(Some((quoted, _))) = state.quote_to_price(pool, target, u64::MAX) else {
                break;
            };
            let rung = PoolRung {
                bps: f64::from(*bps),
                amount_in: units(quoted.amount_in, input),
                amount_out: units(quoted.amount_out, output),
            };
            // A further price takes a larger swap: a rung that doesn't
            // grow (the pool's liquidity ended between two) ends the side.
            let grows = rungs.last().is_none_or(|last| {
                rung.amount_in > last.amount_in && rung.amount_out > last.amount_out
            });
            if !grows {
                break;
            }
            rungs.push(rung);
        }
        rungs
    };
    Reading {
        mid,
        fee_bps: state.fee_fraction() * 10_000.0,
        liquidity: state.liquidity() as f64,
        sells: side(amm::Direction::BaseIn),
        buys: side(amm::Direction::QuoteIn),
    }
}

/// A watched pool, and what was read of it.
pub struct Watched {
    pub pool: amm::Pool,
    /// Every account read for it so far: watched, and in the cache if the
    /// chain holds it.
    accounts: Vec<Pubkey>,
    /// The slot of the pool's write last read, and the slot it was read in.
    written_slot: u64,
    read_slot: u64,
    /// Accounts being read from the node: the pool isn't assembled until
    /// they are in the cache.
    fetching: Option<Fetching>,
}

/// Where a task reading accounts from the node leaves how it went.
pub type Outcome = std::sync::Arc<std::sync::OnceLock<Result<(), String>>>;

/// Accounts being read from the node, off the strategies' loop.
struct Fetching {
    keys: Vec<Pubkey>,
    outcome: Outcome,
    since: Instant,
}

/// How long a read from the node may take before it is given up: twice
/// the node's own timeout for a backfill, past which its task is gone.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Watched {
    pub fn new(pool: amm::Pool) -> Self {
        Self {
            accounts: vec![pool.address],
            pool,
            written_slot: 0,
            read_slot: 0,
            fetching: None,
        }
    }

    /// The accounts read for it, as the cache names them.
    pub fn addresses(&self) -> Vec<mtx_core::feed::Address> {
        self.accounts.iter().map(Pubkey::to_bytes).collect()
    }

    /// Whether to read it in `slot`: a write changed it since, or it is
    /// `every_slots` since the last read.
    pub fn due(&self, slot: u64, written_slot: u64, every_slots: u64) -> bool {
        let is_changed = written_slot > self.written_slot;
        let is_stale = slot >= self.read_slot.saturating_add(every_slots);
        is_changed || is_stale
    }

    /// A task is reading `keys` from the node, and will leave how it went
    /// in the outcome returned: until then, the pool isn't assembled.
    /// Refused past [`POOL_ACCOUNTS_MAX`].
    pub fn fetch(&mut self, keys: Vec<Pubkey>) -> Result<Outcome, String> {
        assert!(self.fetching.is_none(), "one read from the node at a time");
        if self.accounts.len() + keys.len() > POOL_ACCOUNTS_MAX {
            return Err(format!(
                "{}: past {POOL_ACCOUNTS_MAX} accounts watched",
                self.pool.instrument
            ));
        }
        let outcome = Outcome::default();
        self.fetching = Some(Fetching {
            keys,
            outcome: outcome.clone(),
            since: Instant::now(),
        });
        Ok(outcome)
    }

    /// Whether nothing is being read from the node: a read that ended
    /// joins what was read. A read that failed, or that never answered, is
    /// forgotten, and asked for again by the next assembly.
    pub fn fetched(&mut self) -> Result<bool, String> {
        let Some(fetching) = &self.fetching else {
            return Ok(true);
        };
        let outcome = fetching.outcome.get().cloned();
        let is_late = fetching.since.elapsed() > FETCH_TIMEOUT;
        match outcome {
            None if is_late => {
                self.fetching = None;
                Err(format!(
                    "{}: its read from the node never answered",
                    self.pool.instrument
                ))
            }
            None => Ok(false),
            Some(Ok(())) => {
                let fetching = self.fetching.take().expect("checked above");
                self.reads(&fetching.keys)?;
                Ok(true)
            }
            Some(Err(error)) => {
                self.fetching = None;
                Err(format!("{}: {error}", self.pool.instrument))
            }
        }
    }

    /// Accounts read: `more` joins what the pool is assembled from.
    /// Refused past [`POOL_ACCOUNTS_MAX`].
    pub fn reads(&mut self, more: &[Pubkey]) -> Result<(), String> {
        if self.accounts.len() + more.len() > POOL_ACCOUNTS_MAX {
            return Err(format!(
                "{}: past {POOL_ACCOUNTS_MAX} accounts watched",
                self.pool.instrument
            ));
        }
        for key in more {
            assert!(!self.accounts.contains(key), "an account is read once");
            self.accounts.push(*key);
        }
        Ok(())
    }

    /// Its state from `snapshot` (the cache's copy of [`Self::addresses`])
    /// at `timestamp_s`, or the accounts to read first.
    pub fn assemble(&self, snapshot: &Snapshot, timestamp_s: u64) -> Result<Assembly, String> {
        let lookup = |key: &Pubkey| {
            if !self.accounts.contains(key) {
                return Lookup::Unread;
            }
            match snapshot.data(&key.to_bytes()) {
                Some(data) => Lookup::Data(data),
                None => Lookup::Absent,
            }
        };
        amm::State::assemble(&self.pool, timestamp_s, &lookup).map_err(|e| e.to_string())
    }

    /// The event for a read of it in `slot`, of the write in
    /// `written_slot`, that began at `started`.
    pub fn told(
        &mut self,
        state: &amm::State,
        rungs_bps: &[u32],
        slot: u64,
        written_slot: u64,
        started: Instant,
    ) -> Event {
        assert!(written_slot <= slot, "a write read before its slot");
        let reading = reading(&self.pool, state, rungs_bps);
        self.written_slot = written_slot;
        self.read_slot = slot;
        Event::Pool {
            venue: self.pool.instrument.venue.clone(),
            address: self.pool.address.to_string(),
            slot,
            written_slot,
            base: self.pool.base.mint.to_string(),
            quote: self.pool.quote.mint.to_string(),
            mid: reading.mid,
            fee_bps: reading.fee_bps,
            liquidity: reading.liquidity,
            sells: reading.sells,
            buys: reading.buys,
            read_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        }
    }
}

/// The watched pools.
pub struct Pools {
    pub watched: Vec<Watched>,
    pub every_slots: u64,
    pub rungs_bps: Vec<u32>,
}

impl Pools {
    pub fn new(watched: Vec<Watched>, config: &PoolsConfig) -> Self {
        assert!(watched.len() <= POOLS_MAX, "watched pools are bounded");
        Self {
            watched,
            every_slots: config.every_slots.max(EVERY_SLOTS_MIN),
            rungs_bps: config.rungs_bps.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use base64::Engine as _;
    use mtx_core::{Asset, feed::AccountWrite, mandate::Kind};
    use mtx_kit::protocols::{USDC_MINT, orca::whirlpool, raydium::clmm};

    use super::*;
    use crate::live::feed::AccountCache;

    const SOL_MINT: Pubkey = solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");
    const TIMESTAMP_S: u64 = 1_795_000_000;

    #[derive(serde::Deserialize)]
    struct Fixture {
        pool: String,
        accounts: std::collections::BTreeMap<String, FixtureAccount>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureAccount {
        data: String,
    }

    /// A kit fixture's pool (SOL against USDC) and every account of it, in
    /// a cache as the feed would have left them at slot 10.
    fn fixture(name: &str, program: amm::Program) -> (Watched, AccountCache) {
        let path = format!(
            "{}/../kit/tests/fixtures/{name}/accounts.json.gz",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut json = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).expect("fixture"))
            .read_to_string(&mut json)
            .expect("gzip");
        let fixture: Fixture = serde_json::from_str(&json).expect("fixture json");
        let mut cache = AccountCache::default();
        for (key, account) in &fixture.accounts {
            let key: Pubkey = key.parse().expect("key");
            cache.apply(AccountWrite {
                slot: 10,
                address: key.to_bytes(),
                owner: program.id().to_bytes(),
                lamports: 1,
                data: base64::engine::general_purpose::STANDARD
                    .decode(&account.data)
                    .expect("base64"),
                write_version: 0,
            });
        }
        let address: Pubkey = fixture.pool.parse().expect("the pool's key");
        let token = |mint: Pubkey, symbol: &str, decimals: u8| amm::Token {
            mint,
            token_program: Pubkey::default(),
            vault: Pubkey::default(),
            asset: Asset::new(mint.to_string(), symbol, decimals).expect("an asset"),
        };
        let venue = match program {
            amm::Program::Whirlpool => whirlpool::VENUE,
            amm::Program::RaydiumClmm => clmm::VENUE,
        };
        let pool = amm::Pool {
            instrument: InstrumentRef {
                venue: venue.into(),
                kind: Kind::Amm,
                symbol: fixture.pool,
            },
            address,
            program,
            base: token(SOL_MINT, "SOL", 9),
            quote: token(USDC_MINT, "USDC", 6),
        };
        (Watched::new(pool), cache)
    }

    /// Assemble `watched` from `cache` as the runtime does: each round's
    /// missing accounts are "read" (they are in the cache already, or the
    /// chain holds none) and the pool is assembled again.
    fn assembled(watched: &mut Watched, cache: &AccountCache) -> (amm::State, usize) {
        for round in 0..=amm::ASSEMBLY_ROUNDS_MAX {
            let snapshot = cache.snapshot(&watched.addresses());
            match watched.assemble(&snapshot, TIMESTAMP_S).expect("assembles") {
                Assembly::Ready(state) => return (*state, round),
                Assembly::Needs(keys) => watched.reads(&keys).expect("within the bound"),
            }
        }
        panic!("a pool assembles in {} rounds", amm::ASSEMBLY_ROUNDS_MAX);
    }

    fn check_ladder(rungs: &[PoolRung], mid: f64, fee_bps: f64, sells: bool) {
        assert!(rungs.len() >= 5, "a deep pool fills most rungs: {rungs:?}");
        let mut last: Option<(PoolRung, f64)> = None;
        for rung in rungs {
            // Quote per base, fees included: under the mid for a sale,
            // over it for a purchase, and worse by at least the fee.
            let price = if sells {
                rung.amount_out / rung.amount_in
            } else {
                rung.amount_in / rung.amount_out
            };
            let cost_bps = if sells {
                (1.0 - price / mid) * 10_000.0
            } else {
                (price / mid - 1.0) * 10_000.0
            };
            assert!(
                cost_bps >= fee_bps * 0.99 && cost_bps <= fee_bps + rung.bps,
                "a rung of {} bp costs {cost_bps:.2} bp at a {fee_bps} bp fee",
                rung.bps
            );
            if let Some((before, cost_before)) = last {
                assert!(rung.amount_in > before.amount_in && rung.amount_out > before.amount_out);
                // A larger swap gets a worse price: the ladder is concave.
                assert!(cost_bps > cost_before);
            }
            last = Some((*rung, cost_bps));
        }
    }

    #[test]
    fn a_whirlpool_is_read_from_the_cache_in_one_round_and_laddered() {
        let (mut watched, cache) = fixture("orca-whirlpool-sol-usdc", amm::Program::Whirlpool);
        let (state, rounds) = assembled(&mut watched, &cache);
        assert_eq!(rounds, 1, "its tick arrays and oracle, then ready");
        let read = reading(&watched.pool, &state, &default_rungs_bps());
        assert!(
            read.mid > 10.0 && read.mid < 1_000.0,
            "SOL in USDC: {}",
            read.mid
        );
        assert!((read.fee_bps - 4.0).abs() < 1e-9, "the 4 bp pool");
        check_ladder(&read.sells, read.mid, read.fee_bps, true);
        check_ladder(&read.buys, read.mid, read.fee_bps, false);
    }

    #[test]
    fn a_raydium_pool_takes_two_rounds_and_ladders_the_same() {
        let (mut watched, cache) = fixture("raydium-clmm-sol-usdc", amm::Program::RaydiumClmm);
        let (state, rounds) = assembled(&mut watched, &cache);
        assert_eq!(rounds, 2, "its config and extension, then its tick arrays");
        let read = reading(&watched.pool, &state, &default_rungs_bps());
        assert!(read.mid > 10.0 && read.mid < 1_000.0);
        assert!(read.fee_bps > 0.0 && read.fee_bps < 100.0);
        check_ladder(&read.sells, read.mid, read.fee_bps, true);
        check_ladder(&read.buys, read.mid, read.fee_bps, false);
    }

    #[test]
    fn a_read_is_told_with_its_slots_and_makes_the_pool_wait() {
        let (mut watched, cache) = fixture("orca-whirlpool-sol-usdc", amm::Program::Whirlpool);
        // Never read: due at once. The cache's write is of slot 10.
        assert!(watched.due(12, 10, 150));
        let (state, _) = assembled(&mut watched, &cache);
        let event = watched.told(&state, &[10, 20], 12, 10, Instant::now());
        let Event::Pool {
            venue,
            slot,
            written_slot,
            base,
            quote,
            sells,
            buys,
            ..
        } = event
        else {
            panic!("a pool event");
        };
        assert_eq!((venue.as_str(), slot, written_slot), ("orca", 12, 10));
        assert_eq!((base, quote), (SOL_MINT.to_string(), USDC_MINT.to_string()));
        assert_eq!((sells.len(), buys.len()), (2, 2));
        // The same write isn't read again, a later one is, and so is a
        // pool nobody wrote for `every_slots`.
        assert!(!watched.due(13, 10, 150));
        assert!(watched.due(13, 13, 150));
        assert!(watched.due(162, 10, 150));
    }

    #[test]
    fn a_pool_whose_account_is_gone_says_so_and_a_full_one_refuses_more() {
        let (mut watched, _) = fixture("orca-whirlpool-sol-usdc", amm::Program::Whirlpool);
        let empty = AccountCache::default().snapshot(&watched.addresses());
        assert!(watched.assemble(&empty, TIMESTAMP_S).is_err());
        let many: Vec<Pubkey> = (0..POOL_ACCOUNTS_MAX)
            .map(|_| Pubkey::new_unique())
            .collect();
        assert!(watched.reads(&many).is_err());
        assert!(watched.reads(&many[..10]).is_ok());
    }

    #[test]
    fn a_pool_waits_for_its_read_from_the_node_and_forgets_one_that_failed() {
        let (mut watched, cache) = fixture("orca-whirlpool-sol-usdc", amm::Program::Whirlpool);
        assert_eq!(watched.fetched(), Ok(true), "nothing is being read");
        let snapshot = cache.snapshot(&watched.addresses());
        let Assembly::Needs(keys) = watched.assemble(&snapshot, TIMESTAMP_S).unwrap() else {
            panic!("a pool just watched needs its tick arrays");
        };
        // A read that fails: the pool waits, then says why, and asks again.
        let outcome = watched.fetch(keys.clone()).unwrap();
        assert_eq!(watched.fetched(), Ok(false));
        outcome.set(Err("the node didn't answer".into())).unwrap();
        assert!(
            watched
                .fetched()
                .unwrap_err()
                .contains("the node didn't answer")
        );
        assert_eq!(watched.fetched(), Ok(true));
        let snapshot = cache.snapshot(&watched.addresses());
        assert!(matches!(
            watched.assemble(&snapshot, TIMESTAMP_S).unwrap(),
            Assembly::Needs(again) if again == keys
        ));
        // A read that lands: its accounts join, and the pool assembles.
        let outcome = watched.fetch(keys).unwrap();
        outcome.set(Ok(())).unwrap();
        assert_eq!(watched.fetched(), Ok(true));
        let snapshot = cache.snapshot(&watched.addresses());
        assert!(matches!(
            watched.assemble(&snapshot, TIMESTAMP_S).unwrap(),
            Assembly::Ready(_)
        ));
    }

    #[test]
    fn a_configuration_is_bounded() {
        let config = |json: &str| serde_json::from_str::<PoolsConfig>(json).unwrap();
        let pool = r#""orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE""#;
        let good = config(&format!(r#"{{"pools":[{pool}]}}"#));
        assert_eq!(good.validate(), Ok(()));
        assert_eq!(good.every_slots, 150);
        for rungs in ["[]", "[10,10]", "[20,10]", "[0,5]", "[5,4000]"] {
            let bad = config(&format!(r#"{{"pools":[{pool}],"rungs_bps":{rungs}}}"#));
            assert!(bad.validate().is_err(), "{rungs}");
        }
    }
}
