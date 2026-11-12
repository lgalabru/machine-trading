//! Notices: mtx's terminal output, in one style, as pay's CLI writes it. A
//! colored rail (`│`) by level, a bold title, and dimmed lines beneath:
//!
//! ```text
//! │ mtx studio
//! │ neo · flash-crash-spcx
//! │ http://127.0.0.1:58468
//! ```
//!
//! [`NoticeFormat`] renders `tracing` events the same way: the rail by
//! level, the message, and its fields dimmed after it, long ids cut short,
//! instead of `timestamp LEVEL target: message key=value …`.

use std::{
    fmt::{self, Write as _},
    io::IsTerminal,
};

use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

const ACCENT: &str = "\x1b[38;2;162;239;91m";
const BLUE: &str = "\x1b[38;2;122;162;247m";
const YELLOW: &str = "\x1b[38;2;240;198;116m";
const RED: &str = "\x1b[38;2;255;107;107m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// A notice's level: its rail's color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Whether stderr takes colors: a terminal, and no `NO_COLOR`.
fn color() -> bool {
    static COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *COLOR.get_or_init(|| std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn paint(style: &str, text: &str) -> String {
    if color() {
        format!("{style}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn rail(level: NoticeLevel) -> String {
    let style = match level {
        NoticeLevel::Info => BLUE,
        NoticeLevel::Success => ACCENT,
        NoticeLevel::Warning => YELLOW,
        NoticeLevel::Error => RED,
    };
    paint(&format!("{BOLD}{style}"), "│")
}

/// A notice: the rail, the title in bold, each line of `body` dimmed.
pub fn notice(level: NoticeLevel, title: &str, body: &str) -> String {
    let rail = rail(level);
    let mut out = format!("{rail} {}\n", paint(BOLD, title));
    for line in body.lines() {
        let _ = writeln!(out, "{rail} {}", paint(DIM, line));
    }
    out
}

/// Print a notice to stderr.
pub fn print(level: NoticeLevel, title: &str, body: &str) {
    eprint!("{}", notice(level, title, body));
}

/// `tracing` events as notices: the rail by level, the message, its fields.
pub struct NoticeFormat;

impl<S, N> FormatEvent<S, N> for NoticeFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let level = match *event.metadata().level() {
            Level::ERROR => NoticeLevel::Error,
            Level::WARN => NoticeLevel::Warning,
            _ => NoticeLevel::Info,
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut message = fields.message;
        if let Some(first) = message.get(..1) {
            message = first.to_uppercase() + &message[1..];
        }
        write!(writer, "{} {message}", rail(level))?;
        if !fields.rest.is_empty() {
            write!(writer, "  {}", paint(DIM, &fields.rest.join("  ")))?;
        }
        writeln!(writer)
    }
}

/// An event's message, and its other fields as `key value`.
#[derive(Default)]
struct Fields {
    message: String,
    rest: Vec<String>,
}

/// Fields that say nothing to a person reading the log.
const QUIET: &[&str] = &[
    "signature",
    "signatures",
    "log.target",
    "log.module_path",
    "log.file",
    "log.line",
];

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let name = field.name();
        let rendered = format!("{value:?}");
        if name == "message" {
            self.message = rendered;
            return;
        }
        if QUIET.contains(&name) {
            return;
        }
        let value = rendered
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&rendered);
        self.rest
            .push(format!("{} {}", name.replace('_', " "), short(value)));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if !QUIET.contains(&field.name()) {
            self.rest.push(format!(
                "{} {}",
                field.name().replace('_', " "),
                short(value)
            ));
        }
    }
}

/// A long base58 id (a key, a signature) as its ends: `5PWK…Wqbue`.
fn short(value: &str) -> String {
    let base58 = value.len() > 24 && value.chars().all(|c| c.is_ascii_alphanumeric());
    if base58 {
        format!("{}…{}", &value[..4], &value[value.len() - 4..])
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_keeps_every_line_on_its_rail() {
        // Not a terminal under test: no colors.
        assert_eq!(
            notice(NoticeLevel::Info, "mtx studio", "neo\nhttp://127.0.0.1:1"),
            "│ mtx studio\n│ neo\n│ http://127.0.0.1:1\n"
        );
    }

    #[test]
    fn long_ids_are_cut_to_their_ends() {
        assert_eq!(
            short("FYJaR37SqpCMeQm5TKh1fHkWrmGTemJR7EBcDu2V9N9H"),
            "FYJa…9N9H"
        );
        assert_eq!(short("practice-1"), "practice-1");
        assert_eq!(short("10000.0"), "10000.0");
    }
}
