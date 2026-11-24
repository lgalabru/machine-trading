//! The sidecar contract, line by line (`docs/sidecar.md`): every event and
//! every reply as it goes over the wire, fixed here. A change to one of
//! these lines is a change a sidecar sees: it raises
//! `mtx_core::events::PROTOCOL`, or it is a new variant an old reader
//! skips.

use mtx_core::{
    control::{
        CommandKind, ControlError, ErrorCode, FeedStatus, Reply, StrategyState, StrategyStatus,
    },
    events::{Event, Line, Liquidity, PROTOCOL, PaperModel, PauseReason, Side, StepTimings},
};
use serde_json::Value;

fn status() -> StrategyStatus {
    StrategyStatus {
        name: "mm-sol".into(),
        state: StrategyState::Paused {
            why: "the enforcer: drawdown".into(),
        },
        symbol: "SOL".into(),
        authority: "GFKJ2dPLAQ4EYHpbVN5PZWNBnB3rk8ZtYCd5TaZ2HLUk".into(),
        cap: 0.1,
        max_drawdown: 20.0,
        steps: 652,
        skipped: 0,
        shadow: true,
        last_step_ms: Some(2),
        sends_landed: 366,
        sends_failed: 0,
        last_send_error: None,
        last_slot: Some(471945954),
        position: Some(0.0),
        pnl: Some(0.0),
        drawdown: Some(0.0),
        family: None,
        component: None,
    }
}

/// Each event, and the one line it is.
fn lines() -> Vec<(Event, &'static str)> {
    vec![
        (
            Event::Hello {
                protocol: PROTOCOL,
                feed: FeedStatus {
                    slot: 471945955,
                    behind: 0,
                    dropped: 0,
                },
                strategies: vec![status()],
            },
            r#"{"at_us":1,"event":"hello","protocol":2,"feed":{"slot":471945955,"behind":0,"dropped":0},"strategies":[{"name":"mm-sol","state":"paused","why":"the enforcer: drawdown","symbol":"SOL","authority":"GFKJ2dPLAQ4EYHpbVN5PZWNBnB3rk8ZtYCd5TaZ2HLUk","cap":0.1,"max_drawdown":20.0,"steps":652,"skipped":0,"shadow":true,"last_step_ms":2,"sends_landed":366,"sends_failed":0,"last_send_error":null,"last_slot":471945954,"position":0.0,"pnl":0.0,"drawdown":0.0}]}"#,
        ),
        (
            Event::Slot {
                slot: 471945956,
                behind: 1,
                dropped: 0,
            },
            r#"{"at_us":1,"event":"slot","slot":471945956,"behind":1,"dropped":0}"#,
        ),
        (
            Event::Step {
                strategy: "mm-sol".into(),
                slot: 471945956,
                k: 653,
                step_ms: 1,
                timings: StepTimings {
                    feed_us: 5,
                    wake_us: 17,
                    settle_us: 6,
                    read_us: 90,
                    call_us: 70,
                    send_us: 6,
                    total_us: 230,
                },
                answered: true,
                shadow: true,
                orders: 3,
                position: 0.05,
                pnl_usd: -0.01,
                drawdown_usd: 0.02,
                running: true,
            },
            r#"{"at_us":1,"event":"step","strategy":"mm-sol","slot":471945956,"k":653,"step_ms":1,"timings":{"feed_us":5,"wake_us":17,"settle_us":6,"read_us":90,"call_us":70,"send_us":6,"total_us":230},"answered":true,"shadow":true,"orders":3,"position":0.05,"pnl_usd":-0.01,"drawdown_usd":0.02,"running":true}"#,
        ),
        (
            Event::PaperFill {
                strategy: "mm-sol".into(),
                model: PaperModel::Through,
                slot: 471945957,
                side: Side::Buy,
                price: 66.54,
                size: 0.05,
                liquidity: Liquidity::Maker,
                edge_bps: -0.75,
                position: 0.05,
            },
            r#"{"at_us":1,"event":"paper_fill","strategy":"mm-sol","model":"through","slot":471945957,"side":"buy","price":66.54,"size":0.05,"liquidity":"maker","edge_bps":-0.75,"position":0.05}"#,
        ),
        (
            Event::Markout {
                strategy: "mm-sol".into(),
                model: PaperModel::Touch,
                fill_slot: 471945957,
                horizon_slots: 25,
                bps: 1.5,
            },
            r#"{"at_us":1,"event":"markout","strategy":"mm-sol","model":"touch","fill_slot":471945957,"horizon_slots":25,"bps":1.5}"#,
        ),
        (
            Event::Trade {
                symbol: "SOL".into(),
                slot: 471945958,
                taker: Side::Sell,
                price: 66.53,
                size: 2.0,
            },
            r#"{"at_us":1,"event":"trade","symbol":"SOL","slot":471945958,"taker":"sell","price":66.53,"size":2.0}"#,
        ),
        (
            Event::Fate {
                strategy: "mm-sol".into(),
                signature: "5x9yak".into(),
                landed: true,
                slot: Some(471945960),
                error: None,
                after_us: 812_000,
            },
            r#"{"at_us":1,"event":"fate","strategy":"mm-sol","signature":"5x9yak","landed":true,"slot":471945960,"after_us":812000}"#,
        ),
        (
            Event::StepSkipped {
                strategy: "mm-sol".into(),
                slot: 471945961,
            },
            r#"{"at_us":1,"event":"step_skipped","strategy":"mm-sol","slot":471945961}"#,
        ),
        (
            Event::Stopped {
                strategy: "mm-sol".into(),
                why: "trapped at step 9".into(),
            },
            r#"{"at_us":1,"event":"stopped","strategy":"mm-sol","why":"trapped at step 9"}"#,
        ),
        (
            Event::Paused {
                strategy: "mm-sol".into(),
                reason: PauseReason::Mandate,
                why: "drawdown 21 over 20".into(),
            },
            r#"{"at_us":1,"event":"paused","strategy":"mm-sol","reason":"mandate","why":"drawdown 21 over 20"}"#,
        ),
        (
            Event::Command {
                command: CommandKind::Remove,
                error: Some(ControlError::new(
                    ErrorCode::UnknownStrategy,
                    "no strategy b",
                )),
            },
            r#"{"at_us":1,"event":"command","command":"remove","error":{"code":"unknown_strategy","error":"no strategy b"}}"#,
        ),
        (
            Event::Command {
                command: CommandKind::Add,
                error: None,
            },
            r#"{"at_us":1,"event":"command","command":"add"}"#,
        ),
        (
            Event::Command {
                command: CommandKind::Upgrade,
                error: None,
            },
            r#"{"at_us":1,"event":"command","command":"upgrade"}"#,
        ),
        (
            Event::Quote {
                symbol: "MEGA".into(),
                slot: 472500000,
                best_bid: Some(0.04661),
                best_ask: None,
                mark: 0.04666,
            },
            r#"{"at_us":1,"event":"quote","symbol":"MEGA","slot":472500000,"best_bid":0.04661,"best_ask":null,"mark":0.04666}"#,
        ),
        (
            Event::Pool {
                venue: "orca".into(),
                address: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE".into(),
                slot: 472500002,
                written_slot: 472500001,
                base: "So11111111111111111111111111111111111111112".into(),
                quote: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
                mid: 70.25,
                fee_bps: 4.0,
                liquidity: 1.5e15,
                sells: vec![mtx_core::events::PoolRung {
                    bps: 5.0,
                    amount_in: 120.5,
                    amount_out: 8459.75,
                }],
                buys: vec![],
                read_us: 310,
            },
            r#"{"at_us":1,"event":"pool","venue":"orca","address":"Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE","slot":472500002,"written_slot":472500001,"base":"So11111111111111111111111111111111111111112","quote":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","mid":70.25,"fee_bps":4.0,"liquidity":1500000000000000.0,"sells":[{"bps":5.0,"amount_in":120.5,"amount_out":8459.75}],"buys":[],"read_us":310}"#,
        ),
        (
            Event::Obligation {
                venue: "kamino".into(),
                address: "ob1".into(),
                market: "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF".into(),
                slot: 472500002,
                written_slot: 472500001,
                refreshed_slot: 472499000,
                debt_usd: 1000.5,
                limit_usd: 1100.25,
                deposits: vec![mtx_core::events::LendingLeg {
                    mint: "So11111111111111111111111111111111111111112".into(),
                    units: 20.5,
                    value_usd: 1476.0,
                }],
                borrows: vec![],
            },
            r#"{"at_us":1,"event":"obligation","venue":"kamino","address":"ob1","market":"7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF","slot":472500002,"written_slot":472500001,"refreshed_slot":472499000,"debt_usd":1000.5,"limit_usd":1100.25,"deposits":[{"mint":"So11111111111111111111111111111111111111112","units":20.5,"value_usd":1476.0}],"borrows":[]}"#,
        ),
        (
            Event::Reserve {
                venue: "kamino".into(),
                address: "d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q".into(),
                market: "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF".into(),
                mint: "So11111111111111111111111111111111111111112".into(),
                slot: 472500002,
                written_slot: 472500002,
                price: 72.25,
                liquidation_bonus_bps_min: 100.0,
            },
            r#"{"at_us":1,"event":"reserve","venue":"kamino","address":"d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q","market":"7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF","mint":"So11111111111111111111111111111111111111112","slot":472500002,"written_slot":472500002,"price":72.25,"liquidation_bonus_bps_min":100.0}"#,
        ),
        (
            Event::Lagged { dropped: 12 },
            r#"{"at_us":1,"event":"lagged","dropped":12}"#,
        ),
    ]
}

#[test]
fn every_event_is_the_line_the_contract_says() {
    for (event, expected) in lines() {
        let line = Line { at_us: 1, event };
        let written = serde_json::to_string(&line).unwrap();
        assert_eq!(written, expected);
        assert_eq!(serde_json::from_str::<Line>(expected).unwrap(), line);
    }
}

/// A reader skips an event it doesn't know: a new variant isn't a new
/// protocol.
#[test]
fn an_event_this_build_does_not_know_is_an_error_a_reader_can_skip() {
    let future = r#"{"at_us":1,"event":"weather","rain_mm":3}"#;
    assert!(serde_json::from_str::<Line>(future).is_err());
    // Its envelope still reads: the reader knows what it skipped.
    let envelope: Value = serde_json::from_str(future).unwrap();
    assert_eq!(envelope["event"], "weather");
}

/// The upgrade command, as a sidecar writes it.
/// A strategy's family and component are told when they are known, and
/// absent otherwise: an old reader sees the status it always saw.
#[test]
fn a_status_tells_its_family_and_component_only_when_it_has_them() {
    let plain = serde_json::to_value(status()).unwrap();
    assert!(plain.get("family").is_none() && plain.get("component").is_none());
    let named = StrategyStatus {
        family: Some("book-maker".into()),
        component: Some("ab12".into()),
        ..status()
    };
    let line = serde_json::to_value(&named).unwrap();
    assert_eq!(line["family"], "book-maker");
    assert_eq!(line["component"], "ab12");
    let back: StrategyStatus = serde_json::from_value(line).unwrap();
    assert_eq!(back, named);
    let old: StrategyStatus = serde_json::from_value(plain).unwrap();
    assert_eq!(old, status());
}

#[test]
fn an_upgrade_is_the_line_the_contract_says() {
    use mtx_core::control::Command;
    let line = r#"{"command":"upgrade","name":"mm-sol","component":"/home/sol/mtx/new.wasm"}"#;
    let command: Command = serde_json::from_str(line).unwrap();
    assert_eq!(
        command,
        Command::Upgrade {
            name: "mm-sol".into(),
            component: "/home/sol/mtx/new.wasm".into(),
        }
    );
    assert_eq!(serde_json::to_string(&command).unwrap(), line);
    assert_eq!(command.kind(), CommandKind::Upgrade);
    assert!(CommandKind::Upgrade.changes());
}

#[test]
fn every_reply_is_the_line_the_contract_says() {
    let replies = [
        (
            Reply::Ok {
                feed: FeedStatus::default(),
                strategies: Vec::new(),
            },
            r#"{"reply":"ok","feed":{"slot":0,"behind":0,"dropped":0},"strategies":[]}"#,
        ),
        (
            Reply::Error {
                error: ControlError::new(ErrorCode::Exists, "a strategy is already named a"),
            },
            r#"{"reply":"error","code":"exists","error":"a strategy is already named a"}"#,
        ),
    ];
    let markets = Reply::Markets {
        markets: vec![mtx_core::market::MarketInfo {
            instrument: "phoenix:perp:MEGA".parse().unwrap(),
            address: None,
            tick: 0.00001,
            lot: 1.0,
            maker_fee_bps: 0.5,
            taker_fee_bps: 3.5,
            active: true,
        }],
    };
    let line = r#"{"reply":"markets","markets":[{"instrument":"phoenix:perp:MEGA","tick":0.00001,"lot":1.0,"maker_fee_bps":0.5,"taker_fee_bps":3.5,"active":true}]}"#;
    assert_eq!(serde_json::to_string(&markets).unwrap(), line);
    assert_eq!(serde_json::from_str::<Reply>(line).unwrap(), markets);
    assert!(!CommandKind::Markets.changes());
    for (reply, expected) in replies {
        assert_eq!(serde_json::to_string(&reply).unwrap(), expected);
        assert_eq!(serde_json::from_str::<Reply>(expected).unwrap(), reply);
    }
    // Every code reads as it is written.
    for code in [
        ErrorCode::NotACommand,
        ErrorCode::UnknownStrategy,
        ErrorCode::Exists,
        ErrorCode::TooMany,
        ErrorCode::Invalid,
        ErrorCode::Unreadable,
        ErrorCode::StartFailed,
        ErrorCode::OrdersNotCancelled,
        ErrorCode::NotPersisted,
        ErrorCode::Stopped,
        ErrorCode::Failed,
    ] {
        let text = serde_json::to_string(&code).unwrap();
        assert_eq!(serde_json::from_str::<ErrorCode>(&text).unwrap(), code);
    }
}
