//! `mtx live`: control a live runtime while it runs (`docs/rfd/0004`,
//! step 4). Each command is one request to the runtime's control socket
//! ([`mtx_core::control`]), and prints every strategy as the runtime has it
//! after the command.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use mtx_core::{
    Mandate,
    control::{AddStrategy, Command, OnPause, Reply, StrategyState, StrategyStatus},
};

#[derive(Subcommand)]
pub enum LiveCommand {
    /// Every strategy: its state, position, PnL, steps and skipped steps.
    Status,
    /// The markets the runtime watches without trading them
    /// (`control.watch`): tick, lot, fees.
    Markets,
    /// Stop stepping a strategy, or every one. Its resting orders are
    /// cancelled and its position kept, unless told otherwise.
    Pause {
        /// The strategy (default: every one).
        name: Option<String>,
        /// Leave its resting orders.
        #[arg(long, conflicts_with = "flatten")]
        keep_orders: bool,
        /// Cancel its orders and close its position.
        #[arg(long)]
        flatten: bool,
    },
    /// Step a paused strategy again, or every paused one.
    Resume {
        /// The strategy (default: every paused one).
        name: Option<String>,
    },
    /// Give a strategy a new mandate: it restarts under it.
    Mandate {
        name: String,
        /// The mandate file.
        mandate: PathBuf,
    },
    /// Swap a strategy's component for a new build: same name, key and
    /// mandate, no runtime restart. The old one comes back if the new one
    /// doesn't start.
    Upgrade {
        name: String,
        /// The new component (`bot.wasm`), as the node's runtime reads it.
        component: PathBuf,
    },
    /// Start a strategy component under a mandate, with its own key. It
    /// begins paused unless `--run`.
    Add {
        name: String,
        /// The component (`bot.wasm`).
        component: PathBuf,
        /// The mandate file.
        mandate: PathBuf,
        /// The keypair file it trades with.
        #[arg(long)]
        key: PathBuf,
        /// A file holding Phoenix's `/exchange` response: a perp strategy's.
        #[arg(long, required_unless_present = "pool")]
        exchange: Option<PathBuf>,
        /// A pool it trades (`orca:amm:<address>`, `raydium:amm:<address>`),
        /// once each: a strategy on pools, in a shadow run only. `--symbol`
        /// is then what it counts in (`SOL`).
        #[arg(long, requires = "paper_base")]
        pool: Vec<mtx_core::mandate::InstrumentRef>,
        /// What a strategy on pools holds on paper, in its symbol.
        #[arg(long, requires = "pool")]
        paper_base: Option<f64>,
        /// A pool that moved this many bps since its last step is its
        /// step's trigger.
        #[arg(long, requires = "pool", default_value_t = 10.0)]
        trigger_bps: f64,
        /// The family of strategies it is a variant of (`book-maker`): what
        /// a sidecar groups strategies by.
        #[arg(long)]
        family: Option<String>,
        /// Its market, as Phoenix names it (`SOL`).
        #[arg(long)]
        symbol: String,
        /// Slots between its steps.
        #[arg(long, default_value_t = 2)]
        slots_per_step: u64,
        /// Start stepping at once.
        #[arg(long)]
        run: bool,
        /// When the enforcer finds the mandate broken, close the position
        /// too, not only cancel the orders.
        #[arg(long)]
        close_on_breach: bool,
        /// Step, plan and sign, and send nothing: what it would have sent
        /// goes to `<name>.shadow.jsonl` in the runtime's logs.
        #[arg(long)]
        shadow: bool,
    },
    /// Pause a strategy, cancel its orders, end it and forget it.
    Remove { name: String },
    /// The kill switch: pause every strategy and cancel every order.
    Kill,
    /// Follow the runtime's events as they happen, from its event socket
    /// (its configuration's `control.events`): what a sidecar reads
    /// (`docs/sidecar.md`). `--json` prints each line as the runtime sent
    /// it.
    Watch {
        /// The event socket.
        #[arg(long, env = "MTX_LIVE_EVENTS")]
        events: PathBuf,
    },
    /// Measure the runtime: for `--seconds`, each step's stages from the
    /// validator's notice of its slot to its last send, the delay from the
    /// runtime to this reader, and command round trips, as percentiles.
    Latency {
        /// The event socket.
        #[arg(long, env = "MTX_LIVE_EVENTS")]
        events: PathBuf,
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
}

/// Send `command` to the runtime at `socket`, and print what it answers.
pub fn run(socket: &Path, command: LiveCommand, json: bool) -> Result<()> {
    if let LiveCommand::Watch { events } = &command {
        return watch(events, json);
    }
    if let LiveCommand::Latency { events, seconds } = &command {
        return latency(socket, events, *seconds, json);
    }
    let request = to_request(command)?;
    let reply = mtx_core::control::request(socket, &request)
        .with_context(|| format!("the runtime at {}", socket.display()))?;
    match reply {
        Reply::Ok { feed, strategies } => {
            if json {
                let both = serde_json::json!({ "feed": feed, "strategies": strategies });
                println!("{}", serde_json::to_string_pretty(&both)?);
            } else {
                println!(
                    "feed: slot {}, {} behind, {} dropped",
                    feed.slot, feed.behind, feed.dropped
                );
                print!("{}", table(&strategies));
            }
            Ok(())
        }
        Reply::Markets { markets } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&markets)?);
            } else {
                println!("{} watched markets", markets.len());
                for m in &markets {
                    println!(
                        "  {:<24} tick {:<12} lot {:<10} maker {:+.2} bp  taker {:+.2} bp{}",
                        m.instrument.to_string(),
                        m.tick,
                        m.lot,
                        m.maker_fee_bps,
                        m.taker_fee_bps,
                        if m.active { "" } else { "  (not trading)" }
                    );
                }
            }
            Ok(())
        }
        Reply::Error { error } => bail!("the runtime refused: {error}"),
    }
}

/// Print the runtime's events until it closes the connection.
fn watch(socket: &Path, json: bool) -> Result<()> {
    use mtx_core::events::Event;
    let lines = mtx_core::events::subscribe(socket)
        .with_context(|| format!("the runtime's events at {}", socket.display()))?;
    for line in lines {
        let line = line.context("an event line")?;
        if json {
            println!("{}", serde_json::to_string(&line)?);
            continue;
        }
        let text = match &line.event {
            Event::Hello {
                protocol,
                feed,
                strategies,
            } => format!(
                "hello: protocol {protocol}, slot {}, {} strategies",
                feed.slot,
                strategies.len()
            ),
            // A slot a few times a second is noise to a person.
            Event::Slot { .. } => continue,
            Event::Step {
                strategy,
                slot,
                timings,
                orders,
                position,
                pnl_usd,
                ..
            } => format!(
                "{strategy}: step at {slot}, {:.2} ms from the slot (call {:.2}), {orders} orders, position {position}, pnl {pnl_usd:.2}",
                timings.total_us as f64 / 1e3,
                timings.call_us as f64 / 1e3
            ),
            Event::PaperFill {
                strategy,
                model,
                side,
                price,
                size,
                edge_bps,
                ..
            } => format!(
                "{strategy}: paper fill ({model:?}) {side:?} {size} at {price}, edge {edge_bps:+.2} bps"
            ),
            Event::Markout {
                strategy,
                model,
                horizon_slots,
                bps,
                ..
            } => format!("{strategy}: markout ({model:?}) {horizon_slots} slots {bps:+.2} bps"),
            Event::Trade {
                symbol,
                taker,
                price,
                size,
                ..
            } => format!("{symbol}: trade, taker {taker:?} {size} at {price}"),
            Event::Paused {
                strategy,
                reason,
                why,
            } => format!("{strategy}: paused ({reason:?}): {why}"),
            Event::Command { command, error } => match error {
                Some(error) => format!("command {}: refused: {error}", command.as_str()),
                None => format!("command {}: done", command.as_str()),
            },
            Event::StepSkipped { strategy, slot } => {
                format!("{strategy}: step at {slot} skipped, the one before still ran")
            }
            Event::Stopped { strategy, why } => format!("{strategy}: stopped: {why}"),
            Event::State {
                strategy, state, ..
            } => match state {
                StrategyState::Starting { why } => format!("{strategy}: starting: {why}"),
                StrategyState::Running => format!("{strategy}: running"),
                StrategyState::Paused { holds, why } => {
                    let by: Vec<String> = holds
                        .iter()
                        .map(|hold| format!("{hold:?}").to_lowercase())
                        .collect();
                    format!("{strategy}: paused by {}: {why}", by.join(", "))
                }
                StrategyState::Stopped { cause, why } => {
                    let cause = format!("{cause:?}").to_lowercase();
                    format!("{strategy}: stopped ({cause}): {why}")
                }
            },
            Event::Fate {
                strategy,
                signature,
                landed,
                slot,
                error,
                after_us,
            } => match (landed, slot, error) {
                (true, Some(slot), _) => format!(
                    "{strategy}: {signature} landed at {slot}, {:.0} ms after it was sent",
                    *after_us as f64 / 1e3
                ),
                (_, _, Some(error)) => format!("{strategy}: {signature} failed: {error}"),
                _ => format!("{strategy}: {signature} wasn't seen landing"),
            },
            Event::Lagged { dropped } => {
                format!("lagged: {dropped} events lost; run `mtx live status`")
            }
            Event::Quote {
                symbol,
                best_bid,
                best_ask,
                mark,
                ..
            } => {
                let side = |p: &Option<f64>| p.map_or("–".to_string(), |p| format!("{p}"));
                format!(
                    "{symbol}: {} / {} (mark {mark})",
                    side(best_bid),
                    side(best_ask)
                )
            }
            Event::Pool {
                venue,
                address,
                written_slot,
                mid,
                fee_bps,
                sells,
                buys,
                read_us,
                ..
            } => format!(
                "{venue} pool {address}: {mid} at a {fee_bps} bp fee, written in slot \
                 {written_slot}, {} rungs down and {} up, read in {read_us} us",
                sells.len(),
                buys.len()
            ),
            Event::Obligation {
                address,
                debt_usd,
                limit_usd,
                written_slot,
                ..
            } => format!(
                "obligation {address}: ${debt_usd:.0} of debt against a limit of \
                 ${limit_usd:.0}, written in slot {written_slot}"
            ),
            Event::Reserve {
                address,
                mint,
                price,
                ..
            } => format!("reserve {address} of {mint}: ${price}"),
        };
        println!("{text}");
    }
    Ok(())
}

/// Percentiles of a sample, in microseconds.
fn percentiles(name: &str, mut sample: Vec<u64>) -> serde_json::Value {
    sample.sort_unstable();
    let at = |p: f64| -> u64 {
        if sample.is_empty() {
            return 0;
        }
        let i = ((sample.len() - 1) as f64 * p).round() as usize;
        sample[i]
    };
    serde_json::json!({
        "stage": name,
        "n": sample.len(),
        "p50_us": at(0.5),
        "p90_us": at(0.9),
        "p99_us": at(0.99),
        "max_us": sample.last().copied().unwrap_or(0),
    })
}

/// Read the event stream for `seconds` and time each stage; then time
/// command round trips, one connection each and on a kept one.
fn latency(control: &Path, events: &Path, seconds: u64, json: bool) -> Result<()> {
    use mtx_core::events::Event;
    let now_us = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as u64)
    };
    let lines = mtx_core::events::subscribe(events)
        .with_context(|| format!("the runtime's events at {}", events.display()))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut stages: [(&str, Vec<u64>); 7] = [
        ("feed", Vec::new()),
        ("wake", Vec::new()),
        ("settle", Vec::new()),
        ("read", Vec::new()),
        ("call", Vec::new()),
        ("send", Vec::new()),
        ("total", Vec::new()),
    ];
    let mut delivery = Vec::new();
    let (mut skipped, mut lagged) = (0u64, 0u64);
    for line in lines {
        let line = line.context("an event line")?;
        delivery.push(now_us().saturating_sub(line.at_us));
        match line.event {
            Event::Step { timings, .. } => {
                let values = [
                    timings.feed_us,
                    timings.wake_us,
                    timings.settle_us,
                    timings.read_us,
                    timings.call_us,
                    timings.send_us,
                    timings.total_us,
                ];
                for ((_, sample), value) in stages.iter_mut().zip(values) {
                    sample.push(value);
                }
            }
            Event::StepSkipped { .. } => skipped += 1,
            Event::Lagged { dropped } => lagged += dropped,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    let rounds = 100;
    let mut one_shot = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = std::time::Instant::now();
        mtx_core::control::request(control, &Command::Status)?;
        one_shot.push(started.elapsed().as_micros() as u64);
    }
    let mut client = mtx_core::control::Client::connect(control)?;
    let mut kept = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = std::time::Instant::now();
        client.send(&Command::Status)?;
        kept.push(started.elapsed().as_micros() as u64);
    }
    let mut rows: Vec<serde_json::Value> = stages
        .into_iter()
        .map(|(name, sample)| percentiles(name, sample))
        .collect();
    rows.push(percentiles("delivery", delivery));
    rows.push(percentiles("status, one connection each", one_shot));
    rows.push(percentiles("status, kept connection", kept));
    if json {
        let report = serde_json::json!({ "stages": rows, "skipped": skipped, "lagged": lagged });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!(
        "{:<30} {:>7} {:>10} {:>10} {:>10} {:>10}",
        "stage", "n", "p50 µs", "p90 µs", "p99 µs", "max µs"
    );
    for row in &rows {
        println!(
            "{:<30} {:>7} {:>10} {:>10} {:>10} {:>10}",
            row["stage"].as_str().unwrap_or(""),
            row["n"],
            row["p50_us"],
            row["p90_us"],
            row["p99_us"],
            row["max_us"]
        );
    }
    println!("steps skipped: {skipped}, events lost: {lagged}");
    Ok(())
}

fn to_request(command: LiveCommand) -> Result<Command> {
    Ok(match command {
        LiveCommand::Status => Command::Status,
        LiveCommand::Markets => Command::Markets,
        LiveCommand::Watch { .. } | LiveCommand::Latency { .. } => {
            unreachable!("reads events, sends no command")
        }
        LiveCommand::Pause {
            name,
            keep_orders,
            flatten,
        } => Command::Pause {
            name,
            orders: match (keep_orders, flatten) {
                (true, _) => OnPause::Keep,
                (_, true) => OnPause::Flatten,
                _ => OnPause::Cancel,
            },
        },
        LiveCommand::Resume { name } => Command::Resume { name },
        LiveCommand::Mandate { name, mandate } => Command::Mandate {
            name,
            mandate: load_mandate(&mandate)?,
        },
        LiveCommand::Add {
            name,
            component,
            mandate,
            key,
            exchange,
            pool,
            paper_base,
            trigger_bps,
            family,
            symbol,
            slots_per_step,
            run,
            close_on_breach,
            shadow,
        } => Command::Add(AddStrategy {
            name,
            // The runtime reads these files itself, from its own directory.
            component: absolute(&component)?,
            mandate: load_mandate(&mandate)?,
            keypair: absolute(&key)?,
            exchange: match &exchange {
                Some(exchange) => absolute(exchange)?,
                None => PathBuf::new(),
            },
            symbol,
            slots_per_step,
            run,
            close_on_breach,
            shadow,
            pools: paper_base.map(|paper_base| mtx_core::control::PoolsSpec {
                venues: pool,
                paper_base,
                trigger_bps,
            }),
            family,
        }),
        LiveCommand::Remove { name } => Command::Remove { name },
        LiveCommand::Upgrade { name, component } => Command::Upgrade { name, component },
        LiveCommand::Kill => Command::Kill,
    })
}

fn load_mandate(path: &Path) -> Result<Mandate> {
    Mandate::load(path).with_context(|| format!("the mandate {}", path.display()))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("{}", path.display()))
}

/// The strategies, one a line.
fn table(strategies: &[StrategyStatus]) -> String {
    if strategies.is_empty() {
        return "no strategies\n".into();
    }
    let number = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.4}"));
    let mut out = format!(
        "{:<16} {:<10} {:<8} {:>12} {:>12} {:>10} {:>8} {:>8} {:>8}  why\n",
        "name", "state", "market", "position", "pnl", "drawdown", "steps", "skipped", "step ms"
    );
    for s in strategies {
        let (state, why) = match (&s.state, s.shadow) {
            (StrategyState::Running, false) => ("running", ""),
            (StrategyState::Running, true) => ("shadow", ""),
            (StrategyState::Starting { why }, _) => ("starting", why.as_str()),
            (StrategyState::Paused { why, .. }, _) => ("paused", why.as_str()),
            (StrategyState::Stopped { why, .. }, _) => ("stopped", why.as_str()),
        };
        let step_ms = s.last_step_ms.map_or("-".to_string(), |ms| ms.to_string());
        out.push_str(&format!(
            "{:<16} {:<10} {:<8} {:>12} {:>12} {:>10} {:>8} {:>8} {step_ms:>8}  {why}\n",
            s.name,
            state,
            s.symbol,
            number(s.position),
            number(s.pnl),
            number(s.drawdown),
            s.steps,
            s.skipped,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_says_what_becomes_of_the_orders() {
        let pause = |keep_orders, flatten| {
            to_request(LiveCommand::Pause {
                name: None,
                keep_orders,
                flatten,
            })
            .unwrap()
        };
        let orders = |command| match command {
            Command::Pause { orders, .. } => orders,
            other => panic!("{other:?}"),
        };
        assert_eq!(orders(pause(false, false)), OnPause::Cancel);
        assert_eq!(orders(pause(true, false)), OnPause::Keep);
        assert_eq!(orders(pause(false, true)), OnPause::Flatten);
    }

    #[test]
    fn a_table_says_why_a_strategy_isnt_running() {
        let status = StrategyStatus {
            name: "carry".into(),
            state: StrategyState::Paused {
                holds: vec![mtx_core::control::Hold::Kill],
                why: "the kill switch".into(),
            },
            symbol: "SOL".into(),
            authority: "k".into(),
            cap: 1.0,
            max_drawdown: 10.0,
            shadow: false,
            last_step_ms: Some(12),
            sends_landed: 0,
            sends_failed: 0,
            last_send_error: None,
            steps: 3,
            skipped: 1,
            last_slot: Some(9),
            position: Some(0.5),
            pnl: None,
            drawdown: None,
            since_ms: None,
            family: None,
            component: None,
        };
        let table = table(&[status]);
        assert!(table.contains("paused"), "{table}");
        assert!(table.contains("the kill switch"), "{table}");
        assert_eq!(super::table(&[]), "no strategies\n");
    }
}
