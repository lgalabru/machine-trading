//! The replay check: the same grade, run again, comes out the same.
//!
//! A grade is a function of the bot, the snapshot and the scenarios alone.
//! Sessions run on no clock (see [`crate::snapshot::start_offline_stepped`]):
//! the world moves only when the grader steps it, components run on fuel and
//! trusted processes get all the time they need (see
//! [`crate::session::SessionOptions::trusted`]). So a machine's load changes
//! how long a grade takes, never what it says.
//!
//! `mtx grade --replay N` checks that: it runs the grade `N` times, with the
//! machine loaded if asked ([`Load`]), and compares every session's report
//! with the first run's, the reference's sessions included, byte for byte
//! once serialized. The `wall_ms` fields are left out: they measure the
//! machine, not the session. The first field that differs, per session, is
//! reported.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde::Serialize;
use serde_json::Value;

use crate::{Result, grade::GradeReport};

/// The fields that measure the machine rather than the session: every
/// `wall_ms`, the session's and its turns'.
pub const WALL_FIELD: &str = "wall_ms";

/// How a replayed grade went.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReplayCheck {
    /// Runs of the grade, the first included.
    pub runs: usize,
    /// Threads kept busy while it ran (see [`Load`]).
    pub load_threads: usize,
    /// Reports compared each run: the grade's totals, the bot's sessions and
    /// the reference's.
    pub reports: usize,
    pub identical: bool,
    /// Where later runs differed from the first: the first field, per report.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub differences: Vec<Difference>,
}

/// A report that came out differently.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Difference {
    /// The run, counting the first as 1.
    pub run: usize,
    /// `grade`, `bot/<scenario>` or `reference/<scenario>`.
    pub report: String,
    /// The first field that differs, as a path (`flow.sent`,
    /// `positions[0].max`), and its value in the first run and in this one.
    pub field: String,
    pub first: Value,
    pub this: Value,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "run {}, {}: {} was {} and is {}",
            self.run, self.report, self.field, self.first, self.this
        )
    }
}

/// A grade's reports, named, without their wall times: what two runs must
/// agree on.
pub fn reports(grade: &GradeReport) -> Result<Vec<(String, Value)>> {
    let mut totals = serde_json::to_value(grade)?;
    if let Value::Object(fields) = &mut totals {
        fields.remove("scenarios");
        fields.remove("replay");
    }
    let mut reports = vec![("grade".to_string(), totals)];
    for result in &grade.scenarios {
        let name = format!("bot/{}", result.session.scenario);
        reports.push((name, serde_json::to_value(result)?));
    }
    for anchor in &grade.anchors {
        let name = format!("reference/{}", anchor.scenario);
        reports.push((name, serde_json::to_value(anchor)?));
    }
    for (_, report) in &mut reports {
        strip_wall(report);
    }
    Ok(reports)
}

/// Drop every [`WALL_FIELD`] in `value`.
pub fn strip_wall(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.remove(WALL_FIELD);
            fields.values_mut().for_each(strip_wall);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_wall),
        _ => {}
    }
}

/// Run `run`'s reports against the first run's: one [`Difference`] per
/// report whose bytes differ, at its first differing field.
pub fn compare(first: &[(String, Value)], this: &[(String, Value)], run: usize) -> Vec<Difference> {
    let mut differences = Vec::new();
    for index in 0..first.len().max(this.len()) {
        match (first.get(index), this.get(index)) {
            (Some((name, a)), Some((other, b))) if name == other => {
                if bytes(a) != bytes(b) {
                    let (field, first, this) = first_difference(a, b, String::new())
                        .unwrap_or_else(|| (String::new(), a.clone(), b.clone()));
                    differences.push(Difference {
                        run,
                        report: name.clone(),
                        field,
                        first,
                        this,
                    });
                }
            }
            (a, b) => {
                let name = |r: Option<&(String, Value)>| {
                    r.map_or(Value::Null, |(name, _)| Value::String(name.clone()))
                };
                differences.push(Difference {
                    run,
                    report: format!("#{index}"),
                    field: "report".to_string(),
                    first: name(a),
                    this: name(b),
                });
            }
        }
    }
    differences
}

/// The first place `a` and `b` differ, depth first in field order, with the
/// values there.
pub fn first_difference(a: &Value, b: &Value, path: String) -> Option<(String, Value, Value)> {
    let child = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (key, value) in x {
                match y.get(key) {
                    Some(other) => {
                        if let Some(found) = first_difference(value, other, child(key)) {
                            return Some(found);
                        }
                    }
                    None => return Some((child(key), value.clone(), Value::Null)),
                }
            }
            y.iter()
                .find(|(key, _)| !x.contains_key(*key))
                .map(|(key, value)| (child(key), Value::Null, value.clone()))
        }
        (Value::Array(x), Value::Array(y)) => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                if let Some(found) = first_difference(p, q, format!("{path}[{i}]")) {
                    return Some(found);
                }
            }
            (x.len() != y.len()).then(|| {
                (
                    format!("{path}.len()"),
                    Value::from(x.len()),
                    Value::from(y.len()),
                )
            })
        }
        // Numbers compare as they print: the bytes a report holds.
        _ => (bytes(a) != bytes(b)).then(|| (path, a.clone(), b.clone())),
    }
}

/// A value as a report holds it: `-0.0` and `0.0` are different bytes,
/// though equal numbers.
fn bytes(value: &Value) -> String {
    value.to_string()
}

/// CPU load for a replay: `threads` threads spinning until it's dropped, so
/// a grade replays on a machine with no time to spare.
pub struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    pub fn start(threads: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..threads)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x: u64 = 1;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005));
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn wall_times_are_left_out_everywhere() {
        let mut report = json!({
            "pnl_usd": 1.5,
            "wall_ms": 120_000,
            "turns": { "overran": 0, "wall_ms": 900 },
        });
        strip_wall(&mut report);
        assert_eq!(report, json!({ "pnl_usd": 1.5, "turns": { "overran": 0 } }));
    }

    #[test]
    fn the_first_differing_field_is_named() {
        let a = json!({ "pnl_usd": 1.0, "positions": [{ "min": 0.0, "max": 3.0 }], "flow": { "sent": 4 } });
        let b = json!({ "pnl_usd": 1.0, "positions": [{ "min": 0.0, "max": 3.5 }], "flow": { "sent": 4 } });
        assert_eq!(
            first_difference(&a, &b, String::new()),
            Some(("positions[0].max".into(), json!(3.0), json!(3.5)))
        );
        assert_eq!(first_difference(&a, &a, String::new()), None);
        let longer = json!({ "pnl_usd": 1.0, "positions": [{ "min": 0.0, "max": 3.0 }, {}], "flow": { "sent": 4 } });
        assert_eq!(
            first_difference(&a, &longer, String::new()),
            Some(("positions.len()".into(), json!(1), json!(2)))
        );
    }

    #[test]
    fn a_cent_apart_is_a_difference_and_a_wall_time_is_not() {
        let first = vec![
            ("bot/s".to_string(), json!({ "pnl_usd": 10.0 })),
            ("reference/s".to_string(), json!({ "pnl_usd": 20.0 })),
        ];
        let same = first.clone();
        assert!(compare(&first, &same, 2).is_empty());
        let mut off = first.clone();
        off[1].1 = json!({ "pnl_usd": 20.01 });
        let differences = compare(&first, &off, 3);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].report, "reference/s");
        assert_eq!(differences[0].field, "pnl_usd");
        assert_eq!(
            differences[0].to_string(),
            "run 3, reference/s: pnl_usd was 20.0 and is 20.01"
        );
        let mut slower = json!({ "pnl_usd": 10.0, "wall_ms": 5 });
        strip_wall(&mut slower);
        assert!(compare(&first[..1], &[("bot/s".to_string(), slower)], 2).is_empty());
    }

    #[test]
    fn a_missing_session_is_a_difference() {
        let first = vec![
            ("bot/a".to_string(), json!({})),
            ("bot/b".to_string(), json!({})),
        ];
        let differences = compare(&first, &first[..1], 2);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].first, json!("bot/b"));
        assert_eq!(differences[0].this, Value::Null);
    }

    #[test]
    fn load_stops_when_dropped() {
        let load = Load::start(2);
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(load);
    }
}
