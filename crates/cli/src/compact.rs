//! A published release's compact form, and the way back.
//!
//! `mtx bench publish` copies a batch, then compacts it here
//! ([`compact_release`]); `mtx bench compact` does the same to a release
//! published before. Readers see the same JSON either way: [`expand_session`]
//! turns a compact session file back into the one the grader wrote, but
//! for what compacting dropped. A compact release:
//!
//! - keeps no transaction signatures, and the calls' list of sends as a
//!   count: a signature is 88 random characters gzip can't shrink, written
//!   twice, and means nothing once the session's chain is gone;
//! - stores a session's steps, transactions and taker orders as columns, a
//!   list per field, instead of an object per row;
//! - keeps each scenario once, in `scenarios/<hash>.json.gz` at the
//!   release's root: every trial of a batch plays the same hidden set, and
//!   a session file points to its scenario (`{"$ref": "scenarios/…"}`);
//! - keeps each of the bots' source files once: a trial whose file matches
//!   one an earlier trial keeps points to it in [`SHARED_FILES`];
//! - cuts each bot log to its first [`crate::publish::LOG_HEAD_BYTES`] and
//!   last [`crate::publish::LOG_TAIL_BYTES`].

use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Where a published trial lists the files another trial of its release
/// keeps: each path in the trial, with the kept file's path in the release.
pub(crate) const SHARED_FILES: &str = "shared-files.json";
/// The release's scenarios, each once, by content.
pub(crate) const SCENARIOS: &str = "scenarios";
/// A session's tables, stored as columns in a compact release.
const TABLES: &[&str] = &["steps", "transactions", "taker_orders"];
/// What a compact release's `release.json` says it is.
pub(crate) const FORMAT: u64 = 2;

/// What compacting a release saved.
#[derive(Debug, Default, serde::Serialize)]
pub struct Compacted {
    pub sessions: usize,
    pub scenarios: usize,
    pub shared_files: usize,
    pub logs: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Compact a published release in place (see the module's docs). Running
/// it on a compact release changes nothing.
pub fn compact_release(release: &Path) -> Result<Compacted> {
    let mut done = Compacted {
        bytes_before: crate::publish::dir_bytes(release),
        ..Compacted::default()
    };
    let trials = crate::review::discover(release);
    for trial in &trials {
        for role in ["bot", "reference"] {
            let dir = trial.join("verifier/sessions").join(role);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(".json.gz"))
                .collect();
            files.sort();
            for file in files {
                // A file that doesn't read is left as it is: readers skip it.
                let Ok(mut session) = read_gz_json(&file) else {
                    continue;
                };
                let mut changed = compact_session(&mut session);
                if let Some(scenario) = session.get("scenario")
                    && scenario.get("$ref").is_none()
                    && !scenario.is_null()
                {
                    let reference = keep_scenario(release, scenario, &mut done)?;
                    session["scenario"] = json!({ "$ref": reference });
                    changed = true;
                }
                if changed {
                    write_gz(&file, &serde_json::to_vec(&session)?)?;
                    done.sessions += 1;
                }
            }
        }
        let logs = trial.join("verifier/bot");
        if let Ok(entries) = std::fs::read_dir(&logs) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.to_string_lossy().ends_with(".log.gz") {
                    continue;
                }
                let Ok(bytes) = read_gz(&path) else {
                    continue;
                };
                let cut = crate::publish::cut_log(&bytes);
                if cut.len() < bytes.len() {
                    write_gz(&path, &cut)?;
                    done.logs += 1;
                }
            }
        }
    }
    done.shared_files = share_files(release, &trials)?;
    // Say what the release is, for readers and for the next run.
    let meta = release.join("release.json");
    if let Some(mut value) = crate::review::read_json(&meta) {
        value["format"] = json!(FORMAT);
        std::fs::write(&meta, serde_json::to_string_pretty(&value)? + "\n")?;
    }
    done.bytes_after = crate::publish::dir_bytes(release);
    Ok(done)
}

/// Compact one session file's JSON; whether anything changed.
fn compact_session(session: &mut Value) -> bool {
    let mut changed = false;
    let Some(timeline) = session.get_mut("timeline").and_then(Value::as_object_mut) else {
        return false;
    };
    if let Some(transactions) = timeline
        .get_mut("transactions")
        .and_then(Value::as_array_mut)
    {
        for tx in transactions {
            changed |= tx
                .as_object_mut()
                .is_some_and(|tx| tx.remove("signature").is_some());
        }
    }
    if let Some(calls) = timeline.get_mut("calls").and_then(Value::as_object_mut)
        && let Some(sends) = calls.get("sends").and_then(Value::as_array)
    {
        let count = sends.len();
        calls.insert("sends".into(), json!(count));
        changed = true;
    }
    for table in TABLES {
        if let Some(rows) = timeline.get(*table).and_then(Value::as_array)
            && let Some(columns) = to_columns(rows)
        {
            timeline.insert((*table).into(), columns);
            changed = true;
        }
    }
    changed
}

/// Rows of objects as an object of columns: `{"field": [v0, v1, …]}`, and
/// `"$rows"`, the count, so an empty table stays one. A row without a field
/// has `null` in its column. `None` if a row isn't an object.
fn to_columns(rows: &[Value]) -> Option<Value> {
    let mut fields: Vec<String> = Vec::new();
    for row in rows {
        for key in row.as_object()?.keys() {
            if !fields.contains(key) {
                fields.push(key.clone());
            }
        }
    }
    let mut columns = Map::new();
    columns.insert("$rows".into(), json!(rows.len()));
    for field in fields {
        let column: Vec<Value> = rows
            .iter()
            .map(|row| row.get(&field).cloned().unwrap_or(Value::Null))
            .collect();
        columns.insert(field, Value::Array(column));
    }
    Some(Value::Object(columns))
}

/// Columns back to rows, a field left out where its value was `null`.
fn to_rows(columns: &Map<String, Value>) -> Value {
    let rows = columns.get("$rows").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut out: Vec<Map<String, Value>> = vec![Map::new(); rows];
    for (field, column) in columns {
        if field == "$rows" {
            continue;
        }
        for (row, value) in out.iter_mut().zip(column.as_array().into_iter().flatten()) {
            if !value.is_null() {
                row.insert(field.clone(), value.clone());
            }
        }
    }
    Value::Array(out.into_iter().map(Value::Object).collect())
}

/// Write `scenario` once, by its content, under the release's
/// [`SCENARIOS`]; returns its path in the release.
fn keep_scenario(release: &Path, scenario: &Value, done: &mut Compacted) -> Result<String> {
    let bytes = serde_json::to_vec(scenario)?;
    let hash = hex(&Sha256::digest(&bytes))[..16].to_string();
    let rel = format!("{SCENARIOS}/{hash}.json.gz");
    let path = release.join(&rel);
    if !path.is_file() {
        write_gz(&path, &bytes)?;
        done.scenarios += 1;
    }
    Ok(rel)
}

/// Keep each of the trials' source files (`artifacts/app/bot`) once: a
/// file whose bytes an earlier trial keeps is removed and listed in the
/// trial's [`SHARED_FILES`]. Returns how many it removed.
fn share_files(release: &Path, trials: &[PathBuf]) -> Result<usize> {
    let mut kept: HashMap<(String, [u8; 32]), String> = HashMap::new();
    let mut removed = 0;
    for trial in trials {
        let mut pointers: BTreeMap<String, Value> =
            crate::review::read_json(&trial.join(SHARED_FILES))
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
                .into_iter()
                .collect();
        let mut files = Vec::new();
        walk_files(&trial.join("artifacts/app/bot"), &mut files);
        files.sort();
        for file in files {
            let rel = file.strip_prefix(trial)?.to_string_lossy().to_string();
            let bytes = std::fs::read(&file)?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let at = file.strip_prefix(release)?.to_string_lossy().to_string();
            match kept.entry((rel.clone(), digest)) {
                std::collections::hash_map::Entry::Occupied(owner) => {
                    pointers.insert(rel, json!(owner.get()));
                    std::fs::remove_file(&file)?;
                    removed += 1;
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(at);
                }
            }
        }
        if !pointers.is_empty() {
            std::fs::write(
                trial.join(SHARED_FILES),
                serde_json::to_string_pretty(&pointers)? + "\n",
            )?;
        }
    }
    Ok(removed)
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk_files(&path, out),
            Ok(kind) if kind.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// A compact session file's JSON as the grader wrote it, but for what
/// compacting dropped: its tables as rows, and its scenario read from the
/// release (`release`, the directory its `{"$ref"}` is relative to). Any
/// other session is left as it is.
pub(crate) fn expand_session(session: &mut Value, release: Option<&Path>) {
    if let Some(timeline) = session.get_mut("timeline").and_then(Value::as_object_mut) {
        for table in TABLES {
            if let Some(columns) = timeline.get(*table).and_then(Value::as_object) {
                let rows = to_rows(columns);
                timeline.insert((*table).into(), rows);
            }
        }
    }
    if let (Some(release), Some(rel)) = (
        release,
        session["scenario"]["$ref"].as_str().map(str::to_string),
    ) && let Some(path) = inside(release, &rel)
        && let Ok(scenario) = read_gz_json(&path)
    {
        session["scenario"] = scenario;
    }
}

/// `rel` under `root`, if it resolves inside it.
pub(crate) fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let path = root.join(rel).canonicalize().ok()?;
    path.starts_with(&root).then_some(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_gz(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(
        std::fs::File::open(path).with_context(|| path.display().to_string())?,
    )
    .read_to_end(&mut bytes)
    .with_context(|| path.display().to_string())?;
    Ok(bytes)
}

fn read_gz_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&read_gz(path)?).with_context(|| path.display().to_string())
}

fn write_gz(to: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut encoder =
        flate2::write::GzEncoder::new(std::fs::File::create(to)?, flate2::Compression::best());
    encoder.write_all(bytes)?;
    encoder.finish()?.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Value {
        json!({
            "scenario": { "name": "hidden-1", "steps": [1, 2, 3] },
            "timeline": {
                "steps": [{ "k": 0, "mark_usd": 100.0 }, { "k": 1, "mark_usd": 101.0, "note": "x" }],
                "transactions": [
                    { "step": 0, "signature": "5Kd…", "ok": true, "instructions": [{ "program": "phoenix" }] },
                    { "step": 1, "signature": "3Jq…", "ok": false }
                ],
                "taker_orders": [],
                "calls": { "methods": { "getSlot": 3 }, "sends": [{ "step": 0, "signature": "5Kd…" }] }
            }
        })
    }

    #[test]
    fn a_compact_session_expands_to_the_original_but_for_signatures_and_sends() {
        let original = session();
        let mut compact = original.clone();
        assert!(compact_session(&mut compact));
        assert!(compact["timeline"]["steps"].is_object());
        assert_eq!(compact["timeline"]["calls"]["sends"], json!(1));
        let text = compact.to_string();
        assert!(!text.contains("5Kd"), "{text}");
        let mut expanded = compact;
        expand_session(&mut expanded, None);
        let mut want = original;
        for tx in want["timeline"]["transactions"].as_array_mut().unwrap() {
            tx.as_object_mut().unwrap().remove("signature");
        }
        want["timeline"]["calls"]["sends"] = json!(1);
        assert_eq!(expanded, want);
        // Compacting again changes nothing.
        let mut again = expanded.clone();
        compact_session(&mut again);
        expand_session(&mut again, None);
        assert_eq!(again, expanded);
    }

    #[test]
    fn a_release_keeps_scenarios_and_files_once() {
        let release = std::env::temp_dir().join(format!("mtx-compact-{}", uuid::Uuid::new_v4()));
        for trial in ["job/t__a", "job/t__b"] {
            let dir = release.join(trial);
            std::fs::create_dir_all(dir.join("verifier/sessions/bot")).unwrap();
            std::fs::write(dir.join("result.json"), r#"{"trial_name": "t"}"#).unwrap();
            write_gz(
                &dir.join("verifier/sessions/bot/hidden-1.json.gz"),
                session().to_string().as_bytes(),
            )
            .unwrap();
            std::fs::create_dir_all(dir.join("artifacts/app/bot/src")).unwrap();
            std::fs::write(dir.join("artifacts/app/bot/Cargo.lock"), "same").unwrap();
            std::fs::write(dir.join("artifacts/app/bot/src/lib.rs"), trial).unwrap();
        }
        let done = compact_release(&release).unwrap();
        assert_eq!(
            (done.sessions, done.scenarios, done.shared_files),
            (2, 1, 1)
        );
        let b = release.join("job/t__b");
        assert!(!b.join("artifacts/app/bot/Cargo.lock").exists());
        assert!(b.join("artifacts/app/bot/src/lib.rs").exists());
        let pointers = crate::review::read_json(&b.join(SHARED_FILES)).unwrap();
        assert_eq!(
            pointers["artifacts/app/bot/Cargo.lock"],
            json!("job/t__a/artifacts/app/bot/Cargo.lock")
        );
        let mut read = read_gz_json(&b.join("verifier/sessions/bot/hidden-1.json.gz")).unwrap();
        expand_session(&mut read, Some(&release));
        assert_eq!(read["scenario"], session()["scenario"]);
        // A second run finds nothing left to do.
        let again = compact_release(&release).unwrap();
        assert_eq!(
            (again.sessions, again.scenarios, again.shared_files),
            (0, 0, 0)
        );
        std::fs::remove_dir_all(&release).unwrap();
    }
}
