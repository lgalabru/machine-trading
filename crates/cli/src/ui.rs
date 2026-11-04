//! Terminal output for the interactive commands: one spinning line for the
//! step in progress, a checkmark for each step that finished.
//!
//! On a terminal the spinner redraws in place; logs written through
//! [`LogWriter`] clear it first so they never interleave with it. Anywhere
//! else (a pipe, CI) only the finished steps print, without color.

use std::{
    io::{self, IsTerminal, Write},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// `#A2EF5B`, the Machine Trading accent.
const ACCENT: &str = "\x1b[38;2;162;239;91m";
const RED: &str = "\x1b[38;2;255;107;107m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const CLEAR_LINE: &str = "\r\x1b[2K";
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct Ui {
    /// Whether stderr is a terminal: colors and the spinner.
    tty: bool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The step in progress, and when it started if it shows a clock.
    active: Option<(String, Option<Instant>)>,
    frame: usize,
}

static UI: OnceLock<Ui> = OnceLock::new();

fn ui() -> &'static Ui {
    UI.get_or_init(|| {
        let tty = io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        if tty {
            std::thread::Builder::new()
                .name("mtx-spinner".into())
                .spawn(spin)
                .expect("spawn the spinner thread");
        }
        Ui {
            tty,
            state: Mutex::new(State::default()),
        }
    })
}

impl Ui {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn spin() {
    loop {
        std::thread::sleep(Duration::from_millis(80));
        let ui = ui();
        let mut state = ui.state();
        state.frame = (state.frame + 1) % FRAMES.len();
        draw(&state);
    }
}

fn draw(state: &State) {
    let Some((message, since)) = &state.active else {
        return;
    };
    let elapsed = since
        .map(|s| s.elapsed().as_secs())
        .filter(|&s| s >= 3)
        .map(|s| format!(" {DIM}{}{RESET}", clock(s)))
        .unwrap_or_default();
    let mut err = io::stderr().lock();
    let _ = write!(
        err,
        "{CLEAR_LINE}{ACCENT}{}{RESET} {message}{elapsed}",
        FRAMES[state.frame]
    );
    let _ = err.flush();
}

/// `m:ss`.
pub fn clock(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Write a finished line, clearing the spinner first; it redraws on the next
/// frame.
fn line(text: &str) {
    let ui = ui();
    let _state = ui.state();
    let mut err = io::stderr().lock();
    let clear = if ui.tty { CLEAR_LINE } else { "" };
    let _ = writeln!(err, "{clear}{text}");
}

/// Start a step: the spinner shows `message` until [`done`] or [`fail`].
pub fn start(message: impl Into<String>) {
    let ui = ui();
    let message = message.into();
    if ui.tty {
        let mut state = ui.state();
        state.active = Some((message, Some(Instant::now())));
        draw(&state);
    }
}

/// Start a step that keeps its own time, such as a status line with a
/// clock: no elapsed time is added.
pub fn start_untimed(message: impl Into<String>) {
    let ui = ui();
    if ui.tty {
        let mut state = ui.state();
        state.active = Some((message.into(), None));
        draw(&state);
    }
}

/// Change what the step in progress says, keeping its clock.
pub fn update(message: impl Into<String>) {
    let ui = ui();
    if ui.tty {
        let mut state = ui.state();
        let since = state
            .active
            .as_ref()
            .map_or_else(|| Some(Instant::now()), |(_, s)| *s);
        state.active = Some((message.into(), since));
        draw(&state);
    }
}

/// Stop the spinner without printing anything.
pub fn clear() {
    let ui = ui();
    if ui.tty {
        ui.state().active = None;
        let _ = write!(io::stderr(), "{CLEAR_LINE}");
    }
}

/// Finish the step in progress with a checkmark.
pub fn done(message: impl AsRef<str>) {
    finish(
        &format!("{}✓{}", paint(ACCENT), paint(RESET)),
        message.as_ref(),
    );
}

/// Finish the step in progress with a cross.
pub fn fail(message: impl AsRef<str>) {
    finish(
        &format!("{}✗{}", paint(RED), paint(RESET)),
        message.as_ref(),
    );
}

fn finish(mark: &str, message: &str) {
    let ui = ui();
    let took = ui
        .state()
        .active
        .take()
        .and_then(|(_, since)| since)
        .map(|since| since.elapsed().as_secs())
        .filter(|&s| s >= 3)
        .map(|s| format!(" {}{}{}", paint(DIM), clock(s), paint(RESET)))
        .unwrap_or_default();
    line(&format!("{mark} {message}{took}"));
}

/// A dim, indented detail under the last finished step.
pub fn note(message: impl AsRef<str>) {
    line(&format!(
        "  {}{}{}",
        paint(DIM),
        message.as_ref(),
        paint(RESET)
    ));
}

/// `text` in the accent color, on a terminal.
pub fn accent(text: impl AsRef<str>) -> String {
    format!("{}{}{}", paint(ACCENT), text.as_ref(), paint(RESET))
}

/// `text` in bold, on a terminal.
pub fn bold(text: impl AsRef<str>) -> String {
    format!("{}{}{}", paint(BOLD), text.as_ref(), paint(RESET))
}

/// `text` dimmed, on a terminal.
pub fn dim(text: impl AsRef<str>) -> String {
    format!("{}{}{}", paint(DIM), text.as_ref(), paint(RESET))
}

fn paint(code: &'static str) -> &'static str {
    if ui().tty { code } else { "" }
}

/// A `tracing` writer that clears the spinner line before each log line.
pub struct LogWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogLine;

    fn make_writer(&'a self) -> Self::Writer {
        LogLine
    }
}

pub struct LogLine;

impl Write for LogLine {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let ui = ui();
        let _state = ui.state();
        let mut err = io::stderr().lock();
        if ui.tty {
            err.write_all(CLEAR_LINE.as_bytes())?;
        }
        err.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_minutes_and_padded_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(9), "0:09");
        assert_eq!(clock(85), "1:25");
        assert_eq!(clock(600), "10:00");
    }
}
