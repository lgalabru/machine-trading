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
