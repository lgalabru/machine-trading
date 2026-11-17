//! What a reader of the chain could learn of an actor from one value: a
//! size, an offset or a hold that is the same every time, or an informed
//! trader that is never wrong. Each test generates sessions of the library
//! and reads the actors' actions the way a bot reading the chain would.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use mtx_kit::Side;
use solana_pubkey::Pubkey;

use super::{Library, Setting, WalletSeat, tests::Book};
use crate::actors::{Cast, Moment};

/// One action of a wallet, as the pretend books note it.
#[derive(Debug, Clone)]
struct Act {
    slot: u64,
    role: String,
    /// The wallet, `role#n`.
    wallet: String,
    verb: String,
    side: Option<Side>,
    size: f64,
    price: f64,
}

fn parse(slot: u64, line: &str) -> Act {
    let mut words = line.split_whitespace();
    let wallet = words.next().unwrap().to_string();
    let role = wallet.split('#').next().unwrap().to_string();
    let verb = words.next().unwrap().to_string();
    if verb == "cancel" {
        return Act {
            slot,
            role,
            wallet,
            verb,
            side: None,
            size: 0.0,
            price: 0.0,
        };
    }
    let side = match words.next().unwrap() {
        "Bid" => Side::Bid,
        _ => Side::Ask,
    };
    let size = words.next().unwrap().parse().unwrap();
    words.next();
    let price = words.next().unwrap().parse().unwrap();
    Act {
        slot,
        role,
        wallet,
        verb,
        side: Some(side),
        size,
        price,
    }
}

/// A session of the library: every wallet's seat is a pretend book named
/// for its role, all noting into one log.
struct Session {
    acts: Vec<Act>,
    requests: Vec<(String, Option<Side>)>,
}

struct Drive<'a> {
    library: &'a serde_json::Value,
    salt: u8,
    slots: u64,
    /// The touch at a slot.
    touch: &'a dyn Fn(u64) -> (f64, f64),
    /// The scenario's level at a slot.
    level: &'a dyn Fn(u64) -> f64,
    /// Roles whose resting orders trade (fill) at the next step.
    fills: &'a [&'static str],
}

impl Drive<'_> {
    async fn run(&self) -> Session {
        let library: Library = serde_json::from_value(self.library.clone()).unwrap();
        library.validate().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let top = Arc::new(Mutex::new((None, None)));
        let mut positions = Vec::new();
        let seats = library
            .wallets()
            .into_iter()
            .enumerate()
            .map(|(n, (role, i))| {
                let position = Arc::new(Mutex::new(0.0));
                positions.push((role, position.clone()));
                WalletSeat {
                    pubkey: Pubkey::new_from_array([n as u8 + 1; 32]),
                    seat: Box::new(Book {
                        top: top.clone(),
                        log: log.clone(),
                        position,
                        name: format!("{role}#{i}"),
                        lot: 0.0,
                    }),
                }
            })
            .collect();
        let level = {
            let level = self.level;
            // The scenario's level is a plain function of the slot.
            let table: Vec<f64> = (0..self.slots + 1_000).map(level).collect();
            Arc::new(move |s: u64| {
                let l = table[(s as usize).min(table.len() - 1)];
                Moment {
                    at: s,
                    level_usd: l,
                    book_usd: l,
                    fair_usd: None,
                }
            })
        };
        let built = library.build(
            seats,
            Setting {
                symbol: "X".into(),
                base_slot: 0,
                path: level.clone(),
                settlement: None,
                send_reader: None,
                liquidations: None,
                rpc_url: None,
                gateways: Vec::new(),
            },
            &|role, n| {
                let mut seed = [self.salt; 32];
                seed[0] = role.len() as u8;
                seed[1] = n as u8;
                seed[2] = role.bytes().map(|b| b ^ 0x5a).fold(0, u8::wrapping_add);
                seed
            },
        );
        let board = built.rfq.clone();
        let mut cast = Cast::default();
        for actor in built.actors {
            cast.add(actor);
        }
        let mut seen = BTreeMap::new();
        let mut acts = Vec::new();
        for slot in (2..=self.slots).step_by(2) {
            *top.lock().unwrap() = {
                let (bid, ask) = (self.touch)(slot);
                (Some(bid), Some(ask))
            };
            cast.before_blocks().await;
            let now = level(slot);
            cast.after_blocks(now).await;
            if let Some(board) = &board {
                for rfq in board.open() {
                    seen.entry(rfq.id).or_insert(rfq.side);
                }
            }
            let lines = std::mem::take(&mut *log.lock().unwrap());
            for line in lines {
                let act = parse(slot, &line);
                if act.verb == "rest" && self.fills.contains(&act.role.as_str()) {
                    for (role, position) in &positions {
                        if *role == act.role.as_str() {
                            *position.lock().unwrap() += act.size;
                        }
                    }
                }
                acts.push(act);
            }
        }
        Session {
            acts,
            requests: seen.into_iter().collect(),
        }
    }
}

/// The market's own flow: noise sizes are lognormal at sigma 1 around one
/// trade size.
fn scale() -> serde_json::Value {
    serde_json::json!({ "trades_per_minute": 150.0, "trade_size": 1.0, "trade_size_sigma": 1.0 })
}

/// The value as the chain shows it: sizes and prices to three decimals.
fn key(v: f64) -> i64 {
    (v * 1_000.0).round() as i64
}

/// The most common value's share of `values`, to three decimals.
fn mode_share(values: &[f64]) -> f64 {
    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
    for v in values {
        *counts.entry(key(*v)).or_default() += 1;
    }
    *counts.values().max().unwrap() as f64 / values.len() as f64
}

fn quantile(values: &[f64], q: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * q) as usize]
}

/// Sizes of the genuine flow: the noise traders' takes.
fn genuine(acts: &[Act]) -> Vec<f64> {
    acts.iter()
        .filter(|a| a.role == "noise" && a.verb == "take")
        .map(|a| a.size)
        .collect()
}

/// No exact size repeats as the actor's own, and its sizes reach into the
/// range the market's ordinary flow covers.
fn assert_sizes_blend(tells: &mut Vec<String>, what: &str, sizes: &[f64], genuine: &[f64]) {
    assert!(sizes.len() >= 60, "{what}: {} actions", sizes.len());
    let distinct: std::collections::BTreeSet<i64> = sizes.iter().map(|v| key(*v)).collect();
    // More actions only crowd the three-decimal grid, so cap what it asks.
    if (distinct.len() as f64) < (0.5 * sizes.len() as f64).min(200.0) {
        tells.push(format!(
            "{what}: {} distinct sizes of {} actions",
            distinct.len(),
            sizes.len()
        ));
    }
    let share = mode_share(sizes);
    if share > 0.1 {
        tells.push(format!("{what}: one size is {share:.2} of its actions"));
    }
    let ordinary = quantile(genuine, 0.99);
    let inside = sizes.iter().filter(|s| **s <= ordinary).count() as f64 / sizes.len() as f64;
    if inside < 0.2 {
        tells.push(format!(
            "{what}: only {inside:.2} of its sizes are within the flow's range (up to {ordinary:.2})"
        ));
    }
}

fn assert_varies(
    tells: &mut Vec<String>,
    what: &str,
    values: &[f64],
    most_common: f64,
    distinct: usize,
) {
    let kinds: std::collections::BTreeSet<i64> = values.iter().map(|v| key(*v)).collect();
    if kinds.len() < distinct || mode_share(values) > most_common {
        tells.push(format!(
            "{what}: {} distinct of {}, most common {:.2}",
            kinds.len(),
            values.len(),
            mode_share(values)
        ));
    }
}

fn steady(_: u64) -> f64 {
    100.0
}

#[tokio::test]
async fn a_stop_hunters_push_hold_and_overshoot_are_drawn_each_hunt() {
    let mut tells = Vec::new();
    let session = Drive {
        library: &serde_json::json!({
            "scale": scale(),
            "noise": [{}],
            "stop_hunters": [{ "level_step_usd": 1.0 }],
        }),
        salt: 3,
        slots: 60_000,
        touch: &|_| (100.04, 100.08),
        level: &steady,
        fills: &[],
    }
    .run()
    .await;
    let hunter: Vec<&Act> = session
        .acts
        .iter()
        .filter(|a| a.role == "stop-hunter")
        .collect();
    let pushes: Vec<&Act> = hunter
        .iter()
        .copied()
        .filter(|a| a.side == Some(Side::Ask))
        .collect();
    let backs: Vec<&Act> = hunter
        .iter()
        .copied()
        .filter(|a| a.side == Some(Side::Bid))
        .collect();
    assert_eq!(pushes.len(), backs.len());
    let sizes: Vec<f64> = pushes.iter().map(|a| a.size).collect();
    assert_sizes_blend(
        &mut tells,
        "a stop hunter's push",
        &sizes,
        &genuine(&session.acts),
    );
    let holds: Vec<f64> = pushes
        .iter()
        .zip(&backs)
        .map(|(push, back)| (back.slot - push.slot) as f64)
        .collect();
    assert_varies(&mut tells, "its hold", &holds, 0.3, 6);
    let overshoots: Vec<f64> = pushes
        .iter()
        .map(|a| ((100.0 - a.price) * 100.0).round())
        .collect();
    assert_varies(&mut tells, "its overshoot", &overshoots, 0.5, 3);
    let waits: Vec<f64> = backs
        .windows(2)
        .zip(pushes.iter().skip(1))
        .map(|(back, push)| (push.slot - back[0].slot) as f64)
        .collect();
    assert_varies(&mut tells, "its cooldown", &waits, 0.3, 6);
    assert!(tells.is_empty(), "{tells:#?}");
}

#[tokio::test]
async fn a_spoofers_wall_distance_lifetime_and_harvest_are_drawn_each_wall() {
    let mut tells = Vec::new();
    let session = Drive {
        library: &serde_json::json!({
            "scale": scale(),
            "noise": [{}],
            "spoofers": [{ "harvest_mult": 1.0 }],
        }),
        salt: 4,
        slots: 60_000,
        touch: &|_| (100.00, 100.10),
        level: &steady,
        fills: &[],
    }
    .run()
    .await;
    let spoofer: Vec<&Act> = session
        .acts
        .iter()
        .filter(|a| a.role == "spoofer")
        .collect();
    let rests: Vec<&Act> = spoofer
        .iter()
        .copied()
        .filter(|a| a.verb == "rest")
        .collect();
    let cancels: Vec<&Act> = spoofer
        .iter()
        .copied()
        .filter(|a| a.verb == "cancel")
        .collect();
    let sizes: Vec<f64> = rests.iter().map(|a| a.size).collect();
    assert_sizes_blend(
        &mut tells,
        "a spoofer's wall",
        &sizes,
        &genuine(&session.acts),
    );
    let harvests: Vec<f64> = spoofer
        .iter()
        .filter(|a| a.verb == "take")
        .map(|a| a.size)
        .collect();
    assert_sizes_blend(
        &mut tells,
        "a spoofer's harvest",
        &harvests,
        &genuine(&session.acts),
    );
    let lifetimes: Vec<f64> = rests
        .iter()
        .zip(&cancels)
        .map(|(rest, cancel)| (cancel.slot - rest.slot) as f64)
        .collect();
    assert_varies(&mut tells, "its wall's lifetime", &lifetimes, 0.3, 6);
    let distances: Vec<f64> = rests
        .iter()
        .map(|a| ((100.0 - a.price) * 100.0).round())
        .collect();
    assert_varies(&mut tells, "its wall's distance", &distances, 0.5, 3);
    assert!(tells.is_empty(), "{tells:#?}");
}

#[tokio::test]
async fn an_icebergs_slices_and_offset_are_drawn_but_it_reloads_at_one_price() {
    let mut tells = Vec::new();
    let mut sizes = Vec::new();
    let mut offsets = Vec::new();
    let mut flow = Vec::new();
    for salt in 0..80 {
        let session = Drive {
            library: &serde_json::json!({
                "scale": scale(),
                "noise": [{}],
                "icebergs": [{ "parent_mult": 20.0 }],
            }),
            salt,
            slots: 200,
            touch: &|_| (100.00, 100.10),
            level: &steady,
            fills: &["iceberg"],
        }
        .run()
        .await;
        let rests: Vec<&Act> = session
            .acts
            .iter()
            .filter(|a| a.role == "iceberg" && a.verb == "rest")
            .collect();
        assert!(rests.len() >= 2, "it reloads");
        assert!(
            rests.iter().all(|a| a.price == rests[0].price),
            "a reload is at the same price"
        );
        sizes.extend(rests.iter().map(|a| a.size));
        offsets.push(((100.0 - rests[0].price) * 100.0).round());
        flow.extend(genuine(&session.acts));
    }
    assert_sizes_blend(&mut tells, "an iceberg's slice", &sizes, &flow);
    assert_varies(&mut tells, "its offset", &offsets, 0.6, 3);
    assert!(tells.is_empty(), "{tells:#?}");
}

/// How often the informed take against the path's way, over a rising path.
async fn wrong_share(library: serde_json::Value, roles: &str) -> f64 {
    let session = Drive {
        library: &library,
        salt: 9,
        slots: 20_000,
        touch: &|_| (99.0, 101.0),
        level: &|s| 100.0 * (1.0 + 0.0004 * s as f64),
        fills: &[],
    }
    .run()
    .await;
    let trades: Vec<&Act> = session
        .acts
        .iter()
        .filter(|a| a.role == roles && a.verb == "take" && a.slot < 19_000)
        .collect();
    assert!(trades.len() > 500, "{} takes", trades.len());
    trades.iter().filter(|a| a.side == Some(Side::Ask)).count() as f64 / trades.len() as f64
}

#[tokio::test]
async fn informed_traders_are_right_on_average_and_wrong_some_of_the_time() {
    let wrong = wrong_share(
        serde_json::json!({
            "scale": scale(),
            "informed": [{ "horizon_slots": 20, "edge_bps": 30.0 }],
        }),
        "informed",
    )
    .await;
    assert!(
        (0.05..=0.35).contains(&wrong),
        "wrong {wrong:.3} of the time"
    );
}

#[tokio::test]
async fn decoys_that_still_see_ahead_are_as_fallible_as_the_informed() {
    let wrong = wrong_share(
        serde_json::json!({
            "scale": scale(),
            "decoys": [{
                "horizon_slots": 20,
                "edge_bps": 30.0,
                "informed_for": { "from": 19_000, "until": 19_500 },
            }],
        }),
        "decoy",
    )
    .await;
    assert!(
        (0.05..=0.35).contains(&wrong),
        "wrong {wrong:.3} of the time"
    );
}

#[tokio::test]
async fn informed_rfq_clients_are_right_on_average_and_wrong_some_of_the_time() {
    let session = Drive {
        library: &serde_json::json!({
            "scale": scale(),
            "rfq_clients": {
                "clients": 10,
                "informed_share": 1.0,
                "lead_slots": 20,
                "per_100_slots": 20.0,
                "two_way_share": 0.0,
                "deadline_slots": 4,
            },
        }),
        salt: 11,
        slots: 40_000,
        touch: &|_| (99.0, 101.0),
        level: &|s| 100.0 * (1.0 + 0.0004 * s as f64),
        fills: &[],
    }
    .run()
    .await;
    let asked = session.requests.len();
    assert!(asked > 500, "{asked} requests");
    let wrong = session
        .requests
        .iter()
        .filter(|(_, side)| *side == Some(Side::Ask))
        .count() as f64
        / asked as f64;
    assert!(
        (0.05..=0.35).contains(&wrong),
        "wrong {wrong:.3} of the time"
    );
}

/// How often two wallets take the same side in the slots they both trade.
fn agreement(acts: &[Act], a: &str, b: &str) -> f64 {
    let sides = |wallet: &str| -> BTreeMap<u64, Side> {
        acts.iter()
            .filter(|x| x.wallet == wallet && x.verb == "take" && x.slot < 19_000)
            .filter_map(|x| x.side.map(|s| (x.slot, s)))
            .collect()
    };
    let (a, b) = (sides(a), sides(b));
    let both: Vec<_> = a
        .iter()
        .filter_map(|(slot, s)| Some((s, b.get(slot)?)))
        .collect();
    assert!(both.len() > 500, "{} shared slots", both.len());
    both.iter().filter(|(x, y)| x == y).count() as f64 / both.len() as f64
}

#[tokio::test]
async fn informed_wallets_err_on_their_own_as_decoys_do() {
    let seeds = [3u8, 4, 5, 6, 7];
    let (mut pairs, mut crossed) = (Vec::new(), Vec::new());
    for salt in seeds {
        let library = serde_json::json!({
            "scale": scale(),
            "informed": [{ "wallets": 2, "horizon_slots": 20, "edge_bps": 30.0 }],
            "decoys": [{
                "wallets": 2,
                "horizon_slots": 20,
                "edge_bps": 30.0,
                "informed_for": { "from": 19_000, "until": 19_500 },
            }],
        });
        let session = Drive {
            library: &library,
            salt,
            slots: 20_000,
            touch: &|_| (99.0, 101.0),
            level: &|s| 100.0 * (1.0 + 0.0004 * s as f64),
            fills: &[],
        }
        .run()
        .await;
        pairs.push(agreement(&session.acts, "informed#0", "informed#1"));
        for i in ["informed#0", "informed#1"] {
            for d in ["decoy#0", "decoy#1"] {
                crossed.push(agreement(&session.acts, i, d));
            }
        }
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let (within, across) = (mean(&pairs), mean(&crossed));
    assert!(
        (within - across).abs() < 0.05,
        "informed pair agree {within:.3}, informed with decoys {across:.3}"
    );
    assert!(
        (0.68..=0.81).contains(&within),
        "informed pair agree {within:.3}"
    );
}
