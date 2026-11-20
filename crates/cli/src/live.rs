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
        /// A file holding Phoenix's `/exchange` response.
        #[arg(long)]
        exchange: PathBuf,
        /// Its market, as Phoenix names it (`SOL`).
        #[arg(long)]
        symbol: String,
        /// Slots between its steps.
        #[arg(long, default_value_t = 2)]
        slots_per_step: u64,
        /// Start stepping at once.
        #[arg(long)]
        run: bool,
    },
    /// Pause a strategy, cancel its orders, end it and forget it.
    Remove { name: String },
    /// The kill switch: pause every strategy and cancel every order.
    Kill,
}

/// Send `command` to the runtime at `socket`, and print what it answers.
pub fn run(socket: &Path, command: LiveCommand, json: bool) -> Result<()> {
    let request = to_request(command)?;
    let reply = mtx_core::control::request(socket, &request)
        .with_context(|| format!("the runtime at {}", socket.display()))?;
    match reply {
        Reply::Ok { strategies } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&strategies)?);
            } else {
                print!("{}", table(&strategies));
            }
            Ok(())
        }
        Reply::Error { error } => bail!("the runtime refused: {error}"),
    }
}

fn to_request(command: LiveCommand) -> Result<Command> {
    Ok(match command {
        LiveCommand::Status => Command::Status,
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
            symbol,
            slots_per_step,
            run,
        } => Command::Add(AddStrategy {
            name,
            // The runtime reads these files itself, from its own directory.
            component: absolute(&component)?,
            mandate: load_mandate(&mandate)?,
            keypair: absolute(&key)?,
            exchange: absolute(&exchange)?,
            symbol,
            slots_per_step,
            run,
        }),
        LiveCommand::Remove { name } => Command::Remove { name },
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
        "{:<16} {:<10} {:<8} {:>12} {:>12} {:>10} {:>8} {:>8}  why\n",
        "name", "state", "market", "position", "pnl", "drawdown", "steps", "skipped"
    );
    for s in strategies {
        let (state, why) = match &s.state {
            StrategyState::Running => ("running", ""),
            StrategyState::Paused { why } => ("paused", why.as_str()),
            StrategyState::Stopped { why } => ("stopped", why.as_str()),
        };
        out.push_str(&format!(
            "{:<16} {:<10} {:<8} {:>12} {:>12} {:>10} {:>8} {:>8}  {why}\n",
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
                why: "the kill switch".into(),
            },
            symbol: "SOL".into(),
            authority: "k".into(),
            cap: 1.0,
            max_drawdown: 10.0,
            steps: 3,
            skipped: 1,
            last_slot: Some(9),
            position: Some(0.5),
            pnl: None,
            drawdown: None,
        };
        let table = table(&[status]);
        assert!(table.contains("paused"), "{table}");
        assert!(table.contains("the kill switch"), "{table}");
        assert_eq!(super::table(&[]), "no strategies\n");
    }
}
