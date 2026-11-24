//! Lending obligations the runtime watches without touching them
//! (`control.lending`): what a sidecar looking for liquidations needs to
//! see of each, as the chain changes it.
//!
//! The obligations are named in a file (`ops/agave/scan-obligations.py`
//! writes it from one scan of the node): the runtime can't list a
//! program's accounts from the write stream, and a node without an account
//! index pays for each listing with a scan of every account. Each
//! obligation joins the feed's watch list and the account cache, with the
//! reserves it deposits to and borrows from.
//!
//! After the strategies' steps:
//!
//! - a reserve whose price changed is told as a `reserve`: the price the
//!   program reads from its oracle, and its collateral's least bonus;
//! - an obligation a write changed, and a few of the others each slot on a
//!   round-robin (so a sidecar that connects late has them all within
//!   [`LendingConfig::every_slots`]), is told as an `obligation`: its legs
//!   in whole tokens, and where the program last put its debt against its
//!   liquidation limit.
//!
//! Nothing here judges an obligation. A sidecar revalues it at the prices
//! of the moment and decides what is under water.

use std::{collections::BTreeMap, path::PathBuf};

use mtx_core::{
    events::{Event, LendingLeg},
    feed::Address,
};
use serde::Deserialize;
use solana_pubkey::Pubkey;

use super::feed::Accounts;
use crate::venue::kamino::klend::{Obligation, Reserve};

/// The lending program, as venues name it.
const VENUE: &str = "kamino";

/// Obligations watched at most: each is an account in the watch list, and
/// all are looked up in the cache every slot. A market's obligations near
/// their limit with debt worth a fee are a few thousand.
pub const OBLIGATIONS_MAX: usize = 4_096;

/// Reserves read at most: Kamino's main market has about fifty.
pub const RESERVES_MAX: usize = 128;

/// The most bytes of the obligations' file: [`OBLIGATIONS_MAX`] entries
/// of an address and two numbers are under a MiB.
pub const LISTED_BYTES_MAX: u64 = 8 << 20;

/// Slots after which a reserve's price is told again though it didn't
/// change: a minute, so a sidecar that connects has every price within
/// one, where an obligation takes [`LendingConfig::every_slots`].
pub const RESERVE_EVERY_SLOTS: u64 = 150;

/// The fewest slots in which every obligation is told again: telling all
/// of them every few slots would be thousands of events a second.
pub const EVERY_SLOTS_MIN: u64 = 150;

fn default_obligations_max() -> usize {
    2_048
}

/// Ten minutes: a sidecar that connects has every obligation by then.
fn default_every_slots() -> u64 {
    1_500
}

/// What to watch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LendingConfig {
    /// The lending market the obligations are of.
    pub market: String,
    /// The file naming them: `{"obligations": [{"address": "…"}, …]}`, as
    /// `ops/agave/scan-obligations.py` writes it, the least healthy first.
    pub obligations: PathBuf,
    /// How many of them are watched, from the file's start.
    #[serde(default = "default_obligations_max")]
    pub obligations_max: usize,
    /// Every obligation is told once in this many slots, written or not.
    #[serde(default = "default_every_slots")]
    pub every_slots: u64,
}

#[derive(Deserialize)]
struct Listed {
    obligations: Vec<ListedObligation>,
}

#[derive(Deserialize)]
struct ListedObligation {
    address: String,
}

impl LendingConfig {
    pub fn market(&self) -> Result<Pubkey, String> {
        self.market
            .parse()
            .map_err(|_| format!("market {}: not an address", self.market))
    }

    /// The obligations the file names, at most `obligations_max` and
    /// [`OBLIGATIONS_MAX`], once each.
    pub fn listed(&self) -> Result<Vec<Pubkey>, String> {
        let path = &self.obligations;
        let size = std::fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        if size > LISTED_BYTES_MAX {
            return Err(format!("{}: past {LISTED_BYTES_MAX} bytes", path.display()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let listed: Listed =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut addresses: Vec<Pubkey> = Vec::new();
        for obligation in listed
            .obligations
            .iter()
            .take(self.obligations_max.min(OBLIGATIONS_MAX))
        {
            let address: Pubkey = obligation.address.parse().map_err(|_| {
                format!(
                    "{}: {} isn't an address",
                    path.display(),
                    obligation.address
                )
            })?;
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        Ok(addresses)
    }
}

/// A reserve as last read: its decoding, and the slot of the write read.
struct Read {
    reserve: Reserve,
    written_slot: u64,
    /// The price last told and the slot it was told in, if any was.
    told: Option<(f64, u64)>,
}

/// The watched obligations and their reserves.
pub struct Lending {
    market: Pubkey,
    /// Each obligation, and the slot of its write last told.
    obligations: Vec<(Pubkey, u64)>,
    reserves: BTreeMap<Pubkey, Read>,
    /// Reserves asked of the node and not read yet.
    asked: Vec<Pubkey>,
    every_slots: u64,
    cursor: usize,
}

/// What a slot's look at the lending accounts found.
#[derive(Debug, Default, PartialEq)]
pub struct Told {
    pub events: Vec<Event>,
    /// Reserves an obligation names that the cache doesn't hold: to watch
    /// and read from the node.
    pub missing: Vec<Pubkey>,
}

impl Lending {
    pub fn new(market: Pubkey, obligations: Vec<Pubkey>, every_slots: u64) -> Self {
        assert!(
            obligations.len() <= OBLIGATIONS_MAX,
            "watched obligations are bounded"
        );
        Self {
            market,
            obligations: obligations
                .into_iter()
                .map(|address| (address, 0))
                .collect(),
            reserves: BTreeMap::new(),
            asked: Vec::new(),
            every_slots: every_slots.max(EVERY_SLOTS_MIN),
            cursor: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.obligations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.obligations.is_empty()
    }

    /// Every obligation's account, as the cache names them.
    pub fn addresses(&self) -> Vec<Address> {
        self.obligations.iter().map(|(a, _)| a.to_bytes()).collect()
    }

    /// The reserves the watched obligations name, in `accounts`: what to
    /// watch and read beside them. At most [`RESERVES_MAX`].
    pub fn reserves_named(&self, accounts: &dyn Accounts) -> Vec<Pubkey> {
        let mut named: Vec<Pubkey> = Vec::new();
        for (address, _) in &self.obligations {
            let decoded = accounts
                .data(&address.to_bytes())
                .and_then(Obligation::decode);
            for reserve in decoded.iter().flat_map(Obligation::reserves) {
                if !named.contains(&reserve) && named.len() < RESERVES_MAX {
                    named.push(reserve);
                }
            }
        }
        named
    }

    /// Whether `reserve` should be asked of the node now: it isn't read,
    /// wasn't asked for, and there is room for it.
    pub fn ask(&mut self, reserve: Pubkey) -> bool {
        let is_known = self.reserves.contains_key(&reserve) || self.asked.contains(&reserve);
        if is_known || self.reserves.len() + self.asked.len() >= RESERVES_MAX {
            return false;
        }
        self.asked.push(reserve);
        true
    }

    /// Start reading the asked reserves the cache now holds. One that
    /// isn't a reserve is forgotten, and said so.
    pub fn adopt(&mut self, written: &dyn Fn(&Pubkey) -> Option<u64>, accounts: &dyn Accounts) {
        for reserve in std::mem::take(&mut self.asked) {
            let read = written(&reserve).zip(accounts.data(&reserve.to_bytes()));
            let Some((written_slot, data)) = read else {
                self.asked.push(reserve);
                continue;
            };
            if let Err(error) = self.reserve(reserve, data, written_slot) {
                tracing::warn!(%reserve, %error, "a reserve an obligation names can't be read");
            }
        }
    }

    /// Look at the lending accounts in `slot`: `written` says the slot of
    /// an account's last write, `accounts` holds their bytes. Reserves
    /// first, so an obligation told in a slot is valued on that slot's
    /// reserves.
    pub fn tell(
        &mut self,
        slot: u64,
        written: &dyn Fn(&Pubkey) -> Option<u64>,
        accounts: &dyn Accounts,
    ) -> Told {
        let mut told = Told::default();
        self.adopt(written, accounts);
        self.read_reserves(slot, written, accounts, &mut told);
        let count = self.obligations.len();
        if count == 0 {
            return told;
        }
        // Each slot's share of the round-robin, so that all are told in
        // `every_slots`.
        let every = usize::try_from(self.every_slots).unwrap_or(usize::MAX);
        let share = count.div_ceil(every).min(count);
        let start = self.cursor;
        self.cursor = (self.cursor + share) % count;
        for at in 0..count {
            let (address, told_slot) = self.obligations[at];
            let Some(written_slot) = written(&address).filter(|w| *w <= slot) else {
                continue;
            };
            let in_share = (at + count - start) % count < share;
            if written_slot <= told_slot && !in_share {
                continue;
            }
            let Some(data) = accounts.data(&address.to_bytes()) else {
                continue;
            };
            match self.obligation(&address, data, slot, written_slot) {
                Ok(event) => {
                    self.obligations[at].1 = written_slot;
                    told.events.push(event);
                }
                Err(reserve) => {
                    if !told.missing.contains(&reserve) {
                        told.missing.push(reserve);
                    }
                }
            }
        }
        told
    }

    /// Decode the reserves a write changed, and tell those whose price
    /// moved.
    fn read_reserves(
        &mut self,
        slot: u64,
        written: &dyn Fn(&Pubkey) -> Option<u64>,
        accounts: &dyn Accounts,
        told: &mut Told,
    ) {
        for (address, read) in &mut self.reserves {
            let newly_written = written(address).filter(|w| *w <= slot && *w > read.written_slot);
            if let Some(written_slot) = newly_written
                && let Some(reserve) = accounts.data(&address.to_bytes()).and_then(Reserve::decode)
            {
                read.written_slot = written_slot;
                read.reserve = reserve;
            }
            // Told when its price changed, and again every so often: a
            // sidecar that connects late has no price of a reserve that
            // sits still (a stablecoin's) until then.
            let is_due = read.told.is_none_or(|(price, told_slot)| {
                price != read.reserve.price || slot >= told_slot.saturating_add(RESERVE_EVERY_SLOTS)
            });
            if is_due {
                read.told = Some((read.reserve.price, slot));
                told.events.push(reserve_event(
                    &self.market,
                    address,
                    &read.reserve,
                    slot,
                    read.written_slot,
                ));
            }
        }
    }

    /// Start reading `address`, a reserve the cache holds as `data`. Its
    /// price is told at the next look. Refused past [`RESERVES_MAX`], and
    /// for bytes that aren't a reserve.
    pub fn reserve(
        &mut self,
        address: Pubkey,
        data: &[u8],
        written_slot: u64,
    ) -> Result<(), String> {
        if self.reserves.len() >= RESERVES_MAX && !self.reserves.contains_key(&address) {
            return Err(format!("{address}: past {RESERVES_MAX} reserves"));
        }
        let reserve = Reserve::decode(data).ok_or_else(|| format!("{address}: not a reserve"))?;
        self.reserves.insert(
            address,
            Read {
                reserve,
                written_slot,
                told: None,
            },
        );
        Ok(())
    }

    /// The `obligation` event for `data`, or the reserve it names that
    /// isn't read yet.
    fn obligation(
        &self,
        address: &Pubkey,
        data: &[u8],
        slot: u64,
        written_slot: u64,
    ) -> Result<Event, Pubkey> {
        // Bytes that aren't an obligation (a closed account) name no
        // reserve: told as an obligation that holds and owes nothing.
        let obligation = Obligation::decode(data);
        let mut deposits = Vec::new();
        let mut borrows = Vec::new();
        for deposit in obligation.iter().flat_map(|o| &o.deposits) {
            let reserve = &self
                .reserves
                .get(&deposit.reserve)
                .ok_or(deposit.reserve)?
                .reserve;
            deposits.push(LendingLeg {
                mint: reserve.mint.to_string(),
                units: reserve.atoms_to_units(deposit.amount as f64 * reserve.exchange_rate()),
                value_usd: deposit.market_value,
            });
        }
        for borrow in obligation.iter().flat_map(|o| &o.borrows) {
            let reserve = &self
                .reserves
                .get(&borrow.reserve)
                .ok_or(borrow.reserve)?
                .reserve;
            borrows.push(LendingLeg {
                mint: reserve.mint.to_string(),
                units: reserve.atoms_to_units(borrow.amount),
                value_usd: borrow.market_value,
            });
        }
        Ok(Event::Obligation {
            venue: VENUE.into(),
            address: address.to_string(),
            market: self.market.to_string(),
            slot,
            written_slot,
            refreshed_slot: obligation.as_ref().map_or(0, |o| o.last_update_slot),
            debt_usd: obligation
                .as_ref()
                .map_or(0.0, |o| o.borrow_factor_adjusted_debt_value),
            limit_usd: obligation
                .as_ref()
                .map_or(0.0, |o| o.unhealthy_borrow_value),
            deposits,
            borrows,
        })
    }
}

fn reserve_event(
    market: &Pubkey,
    address: &Pubkey,
    reserve: &Reserve,
    slot: u64,
    written_slot: u64,
) -> Event {
    Event::Reserve {
        venue: VENUE.into(),
        address: address.to_string(),
        market: market.to_string(),
        mint: reserve.mint.to_string(),
        slot,
        written_slot,
        price: reserve.price,
        liquidation_bonus_bps_min: f64::from(reserve.min_liquidation_bonus_bps),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use base64::Engine as _;
    use mtx_core::feed::AccountWrite;

    use super::*;
    use crate::{live::feed::AccountCache, venue::kamino::klend};

    const SOL_RESERVE: &str = "d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q";
    const USDC_RESERVE: &str = "D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59";
    const MARKET: &str = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF";
    const SF: f64 = (1u64 << 60) as f64;

    fn key(text: &str) -> Pubkey {
        text.parse().unwrap()
    }

    /// The main market's SOL and USDC reserves, from the snapshot of
    /// 2026-11-21.
    fn reserves() -> Vec<(Pubkey, Vec<u8>)> {
        let path = format!(
            "{}/../../snapshots/atomic-arb-2026-11-21/surfnet.json.gz",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut json = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).expect("the snapshot"))
            .read_to_string(&mut json)
            .expect("gzip");
        let accounts: serde_json::Value = serde_json::from_str(&json).expect("json");
        [SOL_RESERVE, USDC_RESERVE]
            .iter()
            .map(|address| {
                let data = accounts[address]["data"].as_str().expect("the reserve");
                let data = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .expect("base64");
                (key(address), data)
            })
            .collect()
    }

    /// An obligation of 20 SOL of collateral tokens against 1,000 USDC,
    /// valued by the program at $1,400 of collateral, $1,000 of debt and
    /// a limit of $1,050.
    fn obligation(usdc_atoms: f64) -> Vec<u8> {
        let mut data = vec![0u8; klend::OBLIGATION_LEN];
        data[..8].copy_from_slice(&klend::account_tag("Obligation"));
        let put = |data: &mut Vec<u8>, at: usize, bytes: &[u8]| {
            data[at..at + bytes.len()].copy_from_slice(bytes)
        };
        let sf = |value: f64| ((value * SF) as u128).to_le_bytes();
        put(&mut data, 16, &77u64.to_le_bytes());
        put(&mut data, 32, &key(MARKET).to_bytes());
        // The first deposit, at 96: its reserve, collateral tokens, value.
        put(&mut data, 96, &key(SOL_RESERVE).to_bytes());
        put(&mut data, 96 + 32, &20_000_000_000u64.to_le_bytes());
        put(&mut data, 96 + 40, &sf(1_400.0));
        // The first borrow, at 1208: its reserve, atoms owed, value.
        put(&mut data, 1208, &key(USDC_RESERVE).to_bytes());
        put(&mut data, 1208 + 88, &sf(usdc_atoms));
        put(&mut data, 1208 + 104, &sf(usdc_atoms / 1e6));
        put(&mut data, 2208, &sf(usdc_atoms / 1e6));
        put(&mut data, 2256, &sf(1_050.0));
        data
    }

    fn write(cache: &mut AccountCache, address: Pubkey, data: Vec<u8>, slot: u64) {
        cache.apply(AccountWrite {
            slot,
            address: address.to_bytes(),
            owner: klend::PROGRAM_ID.to_bytes(),
            lamports: 1,
            data,
            write_version: 0,
        });
    }

    /// A market watching `obligations`, with both reserves read at slot 5.
    fn watching(obligations: &[Pubkey]) -> (Lending, AccountCache) {
        let mut cache = AccountCache::default();
        let mut lending = Lending::new(key(MARKET), obligations.to_vec(), 1_500);
        for (address, data) in reserves() {
            lending.reserve(address, &data, 5).unwrap();
            write(&mut cache, address, data, 5);
        }
        (lending, cache)
    }

    fn told(lending: &mut Lending, cache: &AccountCache, slot: u64) -> Told {
        lending.tell(slot, &|address| cache.slot(&address.to_bytes()), cache)
    }

    #[test]
    fn reserves_are_told_once_and_an_obligation_in_whole_tokens() {
        let owed = Pubkey::new_unique();
        let (mut lending, mut cache) = watching(&[owed]);
        write(&mut cache, owed, obligation(1_000e6), 6);
        let first = told(&mut lending, &cache, 6);
        assert_eq!(first.missing, vec![]);
        let price_of = |wanted: &str| {
            first.events.iter().find_map(|event| match event {
                Event::Reserve {
                    mint,
                    price,
                    liquidation_bonus_bps_min,
                    ..
                } if mint == wanted => Some((*price, *liquidation_bonus_bps_min)),
                _ => None,
            })
        };
        let (sol_price, sol_bonus) =
            price_of("So11111111111111111111111111111111111111112").expect("SOL's reserve");
        let (usdc_price, _) =
            price_of("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").expect("USDC's reserve");
        assert!(
            (usdc_price - 1.0).abs() < 0.01 && sol_price > 10.0,
            "{usdc_price} {sol_price}"
        );
        assert!(sol_bonus > 0.0);
        // The reserves first, then the obligation, valued on them.
        assert_eq!(first.events.len(), 3);
        let Event::Obligation {
            address,
            written_slot,
            refreshed_slot,
            debt_usd,
            limit_usd,
            deposits,
            borrows,
            ..
        } = &first.events[2]
        else {
            panic!("the obligation last: {:?}", first.events);
        };
        assert_eq!(
            (address.as_str(), *written_slot, *refreshed_slot),
            (owed.to_string().as_str(), 6, 77)
        );
        assert_eq!((*debt_usd, *limit_usd), (1_000.0, 1_050.0));
        // 20 collateral tokens redeem for more than 20 SOL (23.2 on the
        // snapshot's day: the reserve's interest since it opened); the debt
        // is 1,000 USDC.
        assert_eq!((deposits.len(), borrows.len()), (1, 1));
        assert!(
            deposits[0].units > 20.0 && deposits[0].units < 25.0,
            "{}",
            deposits[0].units
        );
        assert_eq!((borrows[0].units, borrows[0].value_usd), (1_000.0, 1_000.0));
        // Nothing changed: nothing is told, but the round-robin's share.
        let quiet = told(&mut lending, &cache, 7);
        // A minute on, both prices are told again, unchanged.
        let again = told(&mut lending, &cache, 6 + RESERVE_EVERY_SLOTS);
        let prices = again
            .events
            .iter()
            .filter(|e| matches!(e, Event::Reserve { .. }))
            .count();
        assert_eq!(prices, 2);
        assert_eq!(
            quiet.events.len(),
            1,
            "one obligation, told each slot of its share"
        );
    }

    #[test]
    fn a_write_is_told_at_once_and_the_rest_within_the_period() {
        let obligations: Vec<Pubkey> = (0..300).map(|_| Pubkey::new_unique()).collect();
        let (mut lending, mut cache) = watching(&obligations);
        for address in &obligations {
            write(&mut cache, *address, obligation(1_000e6), 6);
        }
        // First seen, each is a write to tell.
        let all = told(&mut lending, &cache, 6);
        let count = |told: &Told| {
            told.events
                .iter()
                .filter(|e| matches!(e, Event::Obligation { .. }))
                .count()
        };
        assert_eq!(count(&all), 300);
        // Then one a slot (300 over 1,500 slots), and a written one at once.
        assert_eq!(count(&told(&mut lending, &cache, 7)), 1);
        write(&mut cache, obligations[200], obligation(900e6), 8);
        let after = told(&mut lending, &cache, 8);
        assert_eq!(count(&after), 2);
        assert!(after.events.iter().any(|e| matches!(
            e,
            Event::Obligation { address, debt_usd, .. } if *address == obligations[200].to_string() && *debt_usd == 900.0
        )));
        // Every obligation is told again within the period.
        let mut seen = std::collections::BTreeSet::new();
        for slot in 9..(9 + 300) {
            for event in told(&mut lending, &cache, slot).events {
                if let Event::Obligation { address, .. } = event {
                    seen.insert(address);
                }
            }
        }
        assert_eq!(seen.len(), 300);
    }

    #[test]
    fn a_reserve_not_read_yet_is_asked_for_and_a_closed_obligation_holds_nothing() {
        let owed = Pubkey::new_unique();
        let mut cache = AccountCache::default();
        let mut lending = Lending::new(key(MARKET), vec![owed], 1_500);
        write(&mut cache, owed, obligation(1_000e6), 6);
        let first = told(&mut lending, &cache, 6);
        assert_eq!(first.events, vec![]);
        assert_eq!(first.missing, vec![key(SOL_RESERVE)]);
        // Asked once; when the node's answer is in the cache, it is read,
        // and the obligation is told with the next reserve to ask for.
        assert!(lending.ask(key(SOL_RESERVE)) && !lending.ask(key(SOL_RESERVE)));
        let (sol, data) = reserves().remove(0);
        write(&mut cache, sol, data, 6);
        let second = told(&mut lending, &cache, 7);
        assert!(matches!(&second.events[..], [Event::Reserve { .. }]));
        assert_eq!(second.missing, vec![key(USDC_RESERVE)]);
        assert_eq!(
            lending.reserves_named(&cache),
            vec![key(SOL_RESERVE), key(USDC_RESERVE)]
        );
        // Bytes that aren't a reserve are refused; a closed obligation is
        // told with nothing in it.
        assert!(lending.reserve(key(SOL_RESERVE), &[0u8; 10], 5).is_err());
        write(&mut cache, owed, Vec::new(), 7);
        let closed = told(&mut lending, &cache, 7);
        assert!(matches!(
            &closed.events[..],
            [Event::Obligation { debt_usd, deposits, .. }] if *debt_usd == 0.0 && deposits.is_empty()
        ));
    }

    #[test]
    fn the_file_names_the_obligations_once_each_and_bounded() {
        let dir = std::env::temp_dir().join(format!("mtx-lending-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("obligations.json");
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let config = |max: usize| LendingConfig {
            market: MARKET.into(),
            obligations: path.clone(),
            obligations_max: max,
            every_slots: 1_500,
        };
        assert!(config(10).listed().is_err(), "no file yet");
        let text = format!(
            r#"{{"market":"{MARKET}","obligations":[{{"address":"{a}","health":1.01}},{{"address":"{b}"}},{{"address":"{a}"}}]}}"#
        );
        std::fs::write(&path, text).unwrap();
        assert_eq!(config(10).listed(), Ok(vec![a, b]));
        assert_eq!(config(1).listed(), Ok(vec![a]));
        assert_eq!(config(10).market(), Ok(key(MARKET)));
        std::fs::write(&path, r#"{"obligations":[{"address":"nope"}]}"#).unwrap();
        assert!(config(10).listed().is_err());
    }
}
