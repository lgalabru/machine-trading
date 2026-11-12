//! `mtx bot run`: one practice session of a bot, with what its strategy
//! printed, and with `--debug` step by step: at each step, the market the
//! strategy was handed, what it printed, its order sheet, the orders the
//! grader sent, and what the step cost.
//!
//! A strategy component's world waits for it, so pausing at every step
//! changes nothing in the session: it plays exactly as it would graded.

use std::{
    io::{BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use mtx_grader::{
    grade::{self, GradeOptions},
    scenario::Scenario,
    wasm::{StepObserver, StepTrace},
};
use mtx_kit::{Action, Intent, Session, Side};

pub struct Run<'a> {
    pub dir: &'a Path,
    pub task: Option<&'a str>,
    /// A practice scenario's name (`practice-3`) or a scenario file.
    pub scenario: Option<&'a str>,
    pub build: bool,
    pub debug: bool,
    /// With `debug`: play on at this pace instead of waiting for a key.
    pub auto: Option<Duration>,
    /// With `debug`: show steps from this one on.
    pub from: u64,
    pub repo: Option<&'a Path>,
}

/// `mtx bot run`.
pub async fn run(options: Run<'_>) -> Result<String> {
    let dir = options
        .dir
        .canonicalize()
        .with_context(|| format!("no bot at {}", options.dir.display()))?;
    if options.build {
        crate::bot::build(&dir)?;
    }
    let checked = crate::bot::check(&dir);
    if !checked.problems.is_empty() {
        bail!("the bot can't run:\n  {}", checked.problems.join("\n  "));
    }
    let component = mtx_grader::wasm::component_in(&dir).is_some();
    if options.debug && !component {
        bail!(
            "--debug steps through a strategy component: build the bot into bot.wasm (`just build`)"
        );
    }
    let root = crate::bot::repo(options.repo)?;
    let task = crate::bot::resolve_task(&root, options.task)?;
    let snapshot = crate::bot::task_snapshot(&root, &task)?;
    let path = scenario_path(&root, &task, options.scenario)?;
    let scenario = Scenario::load(&path).with_context(|| path.display().to_string())?;
    let logs = dir.join("target/mtx-run-logs");
    std::fs::create_dir_all(&logs)?;
    let observer = options.debug.then(|| {
        std::sync::Arc::new(Debugger::new(options.auto, options.from))
            as std::sync::Arc<dyn StepObserver>
    });
    if !options.debug {
        eprintln!(
            "running {} on {} ({}); its output follows",
            dir.display(),
            scenario.name,
            task
        );
    }
    let report = grade::grade(GradeOptions {
        snapshot_dir: &snapshot,
        bot_dir: &dir,
        scenarios: std::slice::from_ref(&scenario),
        log_dir: &logs,
        jobs: 1,
        first_bot_user: 0,
        reference_dir: None,
        anchor_jobs: 1,
        regimes: None,
        stop_on_disqualify: false,
        detail_dir: None,
        annotations: None,
        components_only: false,
        trusted: false,
        observer,
    })
    .await?;
    let log = logs.join(format!("{}.log", scenario.name));
    let mut out = String::new();
    if !options.debug {
        let printed = std::fs::read_to_string(&log).unwrap_or_default();
        for line in printed.lines() {
            out.push_str(&format!("{} {line}\n", paint(DIM, "│")));
        }
    }
    let result = &report.scenarios[0];
    let session = &result.session;
    out.push_str(&format!(
        "\n{}  {}   {} {}   {} {}",
        paint(BOLD, &scenario.name),
        signed_usd(session.pnl_usd),
        paint(DIM, "bound"),
        usd(result.bound_pnl_usd),
        paint(DIM, "reference"),
        scenario.reference_pnl_usd.map_or("—".into(), signed_usd),
    ));
    if let Some(reason) = &result.disqualified {
        out.push_str(&format!("\n{} {reason}", paint(RED, "disqualified:")));
    }
    if let Some(stopped) = &session.turns.stopped {
        out.push_str(&format!(
            "\n{} {stopped}",
            paint(RED, "the strategy stopped:")
        ));
    }
    out.push_str(&format!(
        "\n{}",
        paint(DIM, &format!("the strategy's log: {}", log.display()))
    ));
    Ok(out)
}

/// The scenario `mtx bot run` plays: a file, a practice scenario by name, or
/// the task's first.
fn scenario_path(root: &Path, task: &str, asked: Option<&str>) -> Result<PathBuf> {
    let practice = crate::bot::practice_scenarios(root, task)?;
    let Some(asked) = asked else {
        return practice
            .into_iter()
            .next()
            .with_context(|| format!("{task} has no practice scenarios"));
    };
    let path = PathBuf::from(asked);
    if path.is_file() {
        return Ok(path);
    }
    practice
        .into_iter()
        .find(|p| p.file_stem().is_some_and(|s| s == asked))
        .with_context(|| format!("no practice scenario {asked} in {task}, and no file {asked}"))
}

/// `--debug`'s console: a block a step, and a key to go on.
struct Debugger {
    auto: Option<Duration>,
    from: u64,
    /// Waiting for a key at each step: until `c`, or without a terminal.
    pausing: AtomicBool,
    /// Showing steps: until `q`.
    showing: AtomicBool,
    /// One step's prompt at a time.
    prompt: Mutex<()>,
}

impl Debugger {
    fn new(auto: Option<Duration>, from: u64) -> Self {
        let interactive = std::io::stdin().is_terminal() && auto.is_none();
        Self {
            auto,
            from,
            pausing: AtomicBool::new(interactive),
            showing: AtomicBool::new(true),
            prompt: Mutex::new(()),
        }
    }
}

impl StepObserver for Debugger {
    fn started(&self, session: &Session, printed: &[String], failed: Option<&str>) {
        let mut out = String::new();
        out.push_str(&rule(&format!(
            "{} · {} steps of {} slots · slots {}–{}",
            session.symbol,
            session.steps,
            session.slots_per_step,
            session.start_slot,
            session.end_slot
        )));
        out.push_str(&row(
            "session",
            &format!(
                "mark {}   oracle {}   cap {}   drawdown limit {}",
                price(session.start_mark),
                price(session.start_oracle),
                units(session.cap),
                usd(session.max_drawdown)
            ),
        ));
        for (name, value) in &session.vars {
            out.push_str(&row("", &format!("{name} = {value}")));
        }
        out.push_str(&printed_rows(printed));
        if let Some(failed) = failed {
            out.push_str(&row("start", &paint(RED, failed)));
        }
        print!("{out}");
        let _ = std::io::stdout().flush();
    }

    fn step(&self, trace: &StepTrace) {
        let m = &trace.market;
        if m.k < self.from || !self.showing.load(Ordering::Relaxed) {
            return;
        }
        let mut out = String::new();
        out.push_str(&rule(&format!(
            "step {} of {} · slot {}{}",
            m.k + 1,
            trace.steps,
            m.slot,
            if m.last { " · the last" } else { "" }
        )));
        let book = match (m.best_bid, m.best_ask) {
            (Some(bid), Some(ask)) => format!("bid {} · ask {}", price(bid), price(ask)),
            _ => "no book".to_string(),
        };
        out.push_str(&row(
            "market",
            &format!(
                "mark {}   oracle {}   index {}   {book}",
                price(m.mark),
                price(m.oracle),
                price(m.index)
            ),
        ));
        out.push_str(&row(
            "",
            &format!(
                "position {}   resting {}/{}   pnl {}   drawdown {}",
                if m.position.size == 0.0 {
                    "flat".to_string()
                } else {
                    format!(
                        "{} @ {}",
                        signed_units(m.position.size),
                        price(m.position.entry_usd)
                    )
                },
                units(m.resting_bids),
                units(m.resting_asks),
                signed_usd(m.pnl),
                usd(m.drawdown)
            ),
        ));
        out.push_str(&printed_rows(&trace.printed));
        if let Some(failed) = &trace.failed {
            out.push_str(&row("failed", &paint(RED, failed)));
        } else if trace.intents.is_empty() {
            out.push_str(&row("sheet", &paint(DIM, "empty: no orders")));
        } else {
            for (i, intent) in trace.intents.iter().enumerate() {
                out.push_str(&row(
                    if i == 0 { "sheet" } else { "" },
                    &intent_text(intent),
                ));
            }
        }
        for note in &trace.notes {
            out.push_str(&row("cut", &paint(DIM, note)));
        }
        for (i, (action, result)) in trace.sent.iter().enumerate() {
            let line = match result {
                Ok(()) => format!("{} {}", paint(ACCENT, "✓"), action_text(action)),
                Err(e) => format!(
                    "{} {} {}",
                    paint(RED, "✗"),
                    action_text(action),
                    paint(DIM, e)
                ),
            };
            out.push_str(&row(if i == 0 { "sent" } else { "" }, &line));
        }
        out.push_str(&row(
            "cost",
            &paint(
                DIM,
                &format!("{} fuel · {} rpc", fuel(trace.fuel), trace.rpc_calls),
            ),
        ));
        print!("{out}");
        let _ = std::io::stdout().flush();
        if let Some(pace) = self.auto {
            std::thread::sleep(pace);
            return;
        }
        if !self.pausing.load(Ordering::Relaxed) {
            return;
        }
        print!(
            "  {}",
            paint(
                DIM,
                "enter: next step · c: run to the end · q: stop showing steps "
            )
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _prompt = self.prompt.lock().unwrap_or_else(|e| e.into_inner());
        if std::io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
            self.pausing.store(false, Ordering::Relaxed);
            return;
        }
        match line.trim() {
            "c" => self.pausing.store(false, Ordering::Relaxed),
            "q" => {
                self.pausing.store(false, Ordering::Relaxed);
                self.showing.store(false, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

const ACCENT: &str = "\x1b[38;2;162;239;91m";
const RED: &str = "\x1b[38;2;255;107;107m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn color() -> bool {
    static COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *COLOR.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn paint(style: &str, text: &str) -> String {
    if color() && !style.is_empty() {
        format!("{style}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// A heading across the terminal: `━━ text ━━━━`.
fn rule(text: &str) -> String {
    let width = 72usize.saturating_sub(text.chars().count() + 4);
    format!(
        "\n{} {} {}\n",
        paint(ACCENT, "━━"),
        paint(BOLD, text),
        paint(ACCENT, &"━".repeat(width))
    )
}

/// One labelled row of a block.
fn row(label: &str, text: &str) -> String {
    format!(" {} {text}\n", paint(DIM, &format!("{label:<8}")))
}

fn printed_rows(printed: &[String]) -> String {
    printed
        .iter()
        .enumerate()
        .map(|(i, line)| {
            row(
                if i == 0 { "printed" } else { "" },
                &format!("{} {line}", paint(ACCENT, "│")),
            )
        })
        .collect()
}

fn intent_text(intent: &Intent) -> String {
    match intent {
        Intent::Target { position, .. } => format!("target_position({})", signed_units(*position)),
        Intent::Quote { bid, ask, size } => {
            format!(
                "quote(bid {}, ask {}, {})",
                price(*bid),
                price(*ask),
                units(*size)
            )
        }
        Intent::Market { side, size, limit } => match side {
            Side::Bid => format!("buy {} ≤ {}", units(*size), price(*limit)),
            Side::Ask => format!("sell {} ≥ {}", units(*size), price(*limit)),
        },
        Intent::Limit {
            side,
            price: p,
            size,
        } => match side {
            Side::Bid => format!("bid {} @ {}", units(*size), price(*p)),
            Side::Ask => format!("ask {} @ {}", units(*size), price(*p)),
        },
        Intent::CancelAll => "cancel_all".into(),
    }
}

fn action_text(action: &Action) -> String {
    match action {
        Action::Market { side, size, limit } => match side {
            Side::Bid => format!("market buy {} ≤ {}", units(*size), price(*limit)),
            Side::Ask => format!("market sell {} ≥ {}", units(*size), price(*limit)),
        },
        Action::Limit {
            side,
            price: p,
            size,
        } => match side {
            Side::Bid => format!("bid {} @ {}", units(*size), price(*p)),
            Side::Ask => format!("ask {} @ {}", units(*size), price(*p)),
        },
        Action::CancelAll => "cancel all".into(),
    }
}

fn price(usd: f64) -> String {
    format!("{usd:.2}")
}

fn units(n: f64) -> String {
    let text = format!("{n:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn signed_units(n: f64) -> String {
    if n > 0.0 {
        format!("+{}", units(n))
    } else {
        units(n)
    }
}

fn usd(v: f64) -> String {
    format!("${v:.2}")
}

fn signed_usd(v: f64) -> String {
    let text = if v >= 0.0 {
        format!("+${v:.2}")
    } else {
        format!("−${:.2}", -v)
    };
    paint(
        if v > 0.0 {
            ACCENT
        } else if v < 0.0 {
            RED
        } else {
            ""
        },
        &text,
    )
}

fn fuel(units: u64) -> String {
    match units {
        u if u >= 1_000_000_000 => format!("{:.1}B", u as f64 / 1e9),
        u if u >= 1_000_000 => format!("{:.1}M", u as f64 / 1e6),
        u if u >= 1_000 => format!("{:.1}K", u as f64 / 1e3),
        u => u.to_string(),
    }
}
