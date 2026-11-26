//! The anchors' sessions on a drawn set, saved by one grade for later grades
//! of the same set to reuse (`mtx grade --anchors-out`, `--anchors-from`).
//!
//! The skill ladder grades every rung of a set against the same anchors: the
//! reference, the informed bot and the nop bot play the same sessions, drawn
//! from the same secret, with the same keys, in every rung's grade. A grade is
//! a function of its bots, its snapshot and its scenarios alone (see
//! [`crate::replay`]), so those sessions come out the same in each grade.
//! Running them once per set rather than once per rung takes two of every
//! three sessions off a v2 task's ladder.
//!
//! A saved file is reused only by a grade that would have run the same
//! anchors on the same sessions: the [`Identity`] the saving grade recorded
//! must be the reusing grade's, field for field, or the grade is refused
//! before any session runs. The verifier never reuses anchors: it grades one
//! bot per set, and its path doesn't change.
//!
//! What a reusing grade doesn't have is what the anchors' sessions left
//! beside their results: their logs and detail files stay the saving grade's.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Error, Result,
    error::io_error,
    grade::{AnchorSessions, GradeReport, StoredTimeline},
    scenario::Scenario,
    session::SessionReport,
};

/// The saved file's format: bumped when what it holds changes meaning.
pub const FORMAT: u32 = 1;
/// The most a saved file may hold, in bytes. A reference session with its
/// steps and every-slot samples is a few MB; a set of 72 sessions, three
/// anchors each, stays well under this.
pub const SAVED_BYTES_MAX: u64 = 1 << 30;
/// The most an anchor's component (`bot.wasm`) may be, in bytes: far over
/// any strategy built today (a few MB).
pub const COMPONENT_BYTES_MAX: u64 = 256 << 20;
/// The most a snapshot may hold: files, and bytes over all of them. A
/// snapshot today is four files and about 3 MB.
pub const SNAPSHOT_FILES_MAX: usize = 1_024;
pub const SNAPSHOT_BYTES_MAX: u64 = 4 << 30;
/// The most the grader's own executable may be, in bytes, read to hash it.
pub const EXECUTABLE_BYTES_MAX: u64 = 2 << 30;

/// What the anchors' sessions are a function of: two grades with the same
/// identity run the same anchor sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The grader that ran them: its version and its executable.
    pub grader_version: u32,
    pub grader_sha256: String,
    /// The secret the set was drawn from ([`GradeReport::eval_seed_sha256`]):
    /// it also seeds the sessions' keys.
    pub eval_seed_sha256: String,
    /// The scenarios' names, in order.
    pub scenarios: Vec<String>,
    /// The scenarios as drawn, and the latent state the informed bot is
    /// handed in each.
    pub draw_sha256: String,
    /// Every file of the snapshot the sessions start from.
    pub snapshot_sha256: String,
    /// The anchors' components (`bot.wasm`).
    pub reference_sha256: String,
    pub informed_sha256: Option<String>,
    /// Whether the sessions ran under scoring v2's rules (`--v2-sessions`):
    /// sampled every slot, and the nop bot run where doing nothing may pay.
    pub v2_sessions: bool,
}

/// What [`Identity::of`] reads.
pub struct IdentityInputs<'a> {
    pub eval_seed_sha256: &'a str,
    pub scenarios: &'a [Scenario],
    pub latent: Option<&'a [String]>,
    pub snapshot_dir: &'a Path,
    pub reference_dir: &'a Path,
    pub informed_dir: Option<&'a Path>,
    pub v2_sessions: bool,
}

impl Identity {
    /// The identity of the anchor sessions a grade with these inputs runs.
    /// Only components can be anchors whose sessions are reused: their one
    /// file is what they run, where a process bot is whatever its `run`
    /// starts.
    pub fn of(inputs: &IdentityInputs<'_>) -> Result<Self> {
        assert!(!inputs.eval_seed_sha256.is_empty());
        if let Some(latent) = inputs.latent {
            assert_eq!(latent.len(), inputs.scenarios.len());
        }
        let executable = std::env::current_exe().map_err(io_error("the grader's executable"))?;
        let identity = Self {
            grader_version: crate::versions::GRADER_VERSION,
            grader_sha256: file_sha256(&executable, EXECUTABLE_BYTES_MAX)?,
            eval_seed_sha256: inputs.eval_seed_sha256.to_string(),
            scenarios: inputs.scenarios.iter().map(|s| s.name.clone()).collect(),
            draw_sha256: draw_sha256(inputs.scenarios, inputs.latent)?,
            snapshot_sha256: tree_sha256(inputs.snapshot_dir)?,
            reference_sha256: component_sha256(inputs.reference_dir)?,
            informed_sha256: inputs.informed_dir.map(component_sha256).transpose()?,
            v2_sessions: inputs.v2_sessions,
        };
        assert_eq!(identity.scenarios.len(), inputs.scenarios.len());
        Ok(identity)
    }

    /// Refuse anchors saved under `saved` for a grade whose identity is
    /// `self`, saying the first thing that differs.
    pub fn check(&self, saved: &Identity) -> Result<()> {
        let differs = |what: &str| -> Result<()> {
            Err(Error::SavedAnchors(format!(
                "{what} differs from the grade that saved them: grade the anchors again \
                 (--anchors-out)"
            )))
        };
        if self.grader_version != saved.grader_version || self.grader_sha256 != saved.grader_sha256
        {
            return differs("the grader");
        }
        if self.eval_seed_sha256 != saved.eval_seed_sha256 {
            return differs("the set's secret (eval_seed_sha256)");
        }
        if self.scenarios.len() != saved.scenarios.len() {
            return differs(&format!(
                "the set's size ({} sessions, saved {})",
                self.scenarios.len(),
                saved.scenarios.len()
            ));
        }
        let mismatch = self
            .scenarios
            .iter()
            .zip(&saved.scenarios)
            .position(|(a, b)| a != b);
        if let Some(index) = mismatch {
            return differs(&format!(
                "session {}'s scenario ({}, saved {})",
                index + 1,
                self.scenarios[index],
                saved.scenarios[index]
            ));
        }
        if self.draw_sha256 != saved.draw_sha256 {
            return differs("the scenarios as drawn");
        }
        if self.snapshot_sha256 != saved.snapshot_sha256 {
            return differs("the snapshot");
        }
        if self.reference_sha256 != saved.reference_sha256 {
            return differs("the reference's component");
        }
        match (&self.informed_sha256, &saved.informed_sha256) {
            (Some(_), None) => return differs("whether an informed bot ran (none was saved)"),
            (None, Some(_)) => return differs("whether an informed bot ran (one was saved)"),
            (Some(a), Some(b)) if a != b => return differs("the informed bot's component"),
            _ => {}
        }
        if self.v2_sessions != saved.v2_sessions {
            return differs("the session rules (--v2-sessions)");
        }
        // Every field compared above: the identities are the same.
        assert_eq!(self, saved);
        Ok(())
    }
}

/// One anchor session as saved: its report and the steps its timeline held
/// (what scoring reads of it: the reference's marks).
#[derive(Debug, Serialize, Deserialize)]
pub struct SavedSession {
    pub report: SessionReport,
    pub timeline: StoredTimeline,
}

impl SavedSession {
    pub(crate) fn of(report: &SessionReport) -> Self {
        let timeline = &report.timeline;
        Self {
            report: report.clone(),
            timeline: StoredTimeline {
                start_slot: timeline.start_slot,
                end_slot: timeline.end_slot,
                slots_per_step: timeline.slots_per_step,
                start_mark_usd: timeline.start_mark_usd,
                steps: timeline.steps.clone(),
            },
        }
    }

    pub(crate) fn into_report(self) -> SessionReport {
        let mut report = self.report;
        report.timeline = self.timeline.into_timeline();
        report
    }
}

/// A saved file: the identity the anchors ran under, and their sessions.
#[derive(Debug, Serialize, Deserialize)]
pub struct SavedAnchors {
    pub format: u32,
    pub identity: Identity,
    pub reference: Vec<SavedSession>,
    pub informed: Option<Vec<SavedSession>>,
    /// The nop bot's PnL per session, where it ran.
    pub nops: Vec<Option<f64>>,
}

impl SavedAnchors {
    /// The anchors `report`'s grade ran, under `identity`. Refused for a
    /// grade that didn't run every anchor session: it stopped early, or it
    /// ran no reference.
    pub fn of(identity: Identity, report: &GradeReport) -> Result<Self> {
        let count = identity.scenarios.len();
        if report.truncated.is_some() || report.scenarios.len() != count {
            return Err(Error::SavedAnchors(
                "the grade stopped before every session: its anchors aren't the set's".into(),
            ));
        }
        if report.anchors.len() != count {
            return Err(Error::SavedAnchors(
                "the grade ran no reference: there are no anchors to save".into(),
            ));
        }
        let informed = identity.informed_sha256.is_some().then(|| {
            assert_eq!(report.informed_anchors.len(), count);
            report
                .informed_anchors
                .iter()
                .map(SavedSession::of)
                .collect()
        });
        let saved = Self {
            format: FORMAT,
            reference: report.anchors.iter().map(SavedSession::of).collect(),
            informed,
            nops: report.scenarios.iter().map(|s| s.nop_pnl_usd).collect(),
            identity,
        };
        saved.check_shape()?;
        Ok(saved)
    }

    /// Write the file at `path`, whole or not at all: a grade killed while
    /// writing leaves no file a later grade could read half of.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec(self)?;
        assert!(
            (json.len() as u64) <= SAVED_BYTES_MAX,
            "saved anchors over their limit"
        );
        let partial = PathBuf::from(format!("{}.partial", path.display()));
        std::fs::write(&partial, &json).map_err(io_error(&partial))?;
        std::fs::rename(&partial, path).map_err(io_error(path))?;
        Ok(())
    }

    /// The anchors saved at `path`, for a grade whose identity is
    /// `identity`: refused unless the saving grade's was the same.
    pub fn load(path: &Path, identity: &Identity) -> Result<AnchorSessions> {
        let bytes = read_bounded(path, SAVED_BYTES_MAX)?;
        let saved: Self = serde_json::from_slice(&bytes).map_err(|e| {
            Error::SavedAnchors(format!(
                "{} isn't a saved anchors file: {e}",
                path.display()
            ))
        })?;
        if saved.format != FORMAT {
            return Err(Error::SavedAnchors(format!(
                "{} is format {}; this grader reads format {FORMAT}: grade the anchors again \
                 (--anchors-out)",
                path.display(),
                saved.format
            )));
        }
        identity.check(&saved.identity)?;
        saved.check_shape()?;
        Ok(saved.into_sessions())
    }

    /// Every anchor has one session per scenario, in the scenarios' order.
    fn check_shape(&self) -> Result<()> {
        let names = &self.identity.scenarios;
        let in_order = |sessions: &[SavedSession]| {
            sessions.len() == names.len()
                && sessions
                    .iter()
                    .zip(names)
                    .all(|(s, name)| &s.report.scenario == name)
        };
        if !in_order(&self.reference) {
            return Err(Error::SavedAnchors(
                "the reference's sessions aren't the set's, in order".into(),
            ));
        }
        match (&self.informed, &self.identity.informed_sha256) {
            (Some(informed), Some(_)) if in_order(informed) => {}
            (None, None) => {}
            _ => {
                return Err(Error::SavedAnchors(
                    "the informed bot's sessions aren't the set's, in order".into(),
                ));
            }
        }
        if self.nops.len() != names.len() {
            return Err(Error::SavedAnchors(
                "the nop bot's sessions aren't the set's".into(),
            ));
        }
        Ok(())
    }

    fn into_sessions(self) -> AnchorSessions {
        AnchorSessions {
            reference: self
                .reference
                .into_iter()
                .map(SavedSession::into_report)
                .collect(),
            informed: self.informed.map(|sessions| {
                sessions
                    .into_iter()
                    .map(SavedSession::into_report)
                    .collect()
            }),
            nops: self.nops,
        }
    }
}

/// SHA-256 of the scenarios and the latent state, each as JSON with its
/// keys sorted (a `serde_json::Value`'s map is ordered): the same draw
/// hashes the same in every process.
fn draw_sha256(scenarios: &[Scenario], latent: Option<&[String]>) -> Result<String> {
    let mut parts: Vec<(String, Vec<u8>)> = Vec::with_capacity(scenarios.len() * 2);
    for (index, scenario) in scenarios.iter().enumerate() {
        let value = serde_json::to_value(scenario)?;
        parts.push((format!("scenario {index}"), serde_json::to_vec(&value)?));
        if let Some(latent) = latent {
            parts.push((format!("latent {index}"), latent[index].as_bytes().to_vec()));
        }
    }
    Ok(crate::versions::hash_parts(
        parts
            .iter()
            .map(|(label, bytes)| (label.as_str(), bytes.as_slice())),
    ))
}

/// SHA-256 of an anchor's component. A process bot is refused: see
/// [`Identity::of`].
fn component_sha256(bot_dir: &Path) -> Result<String> {
    match crate::wasm::component_in(bot_dir) {
        Some(component) => file_sha256(&component, COMPONENT_BYTES_MAX),
        None => Err(Error::SavedAnchors(format!(
            "{} has no bot.wasm: only a component anchor's sessions can be saved and reused",
            bot_dir.display()
        ))),
    }
}

/// SHA-256 over every regular file under `dir`, by relative path, in path
/// order. Bounded in files and bytes: past either, refused.
fn tree_sha256(dir: &Path) -> Result<String> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).map_err(io_error(&at))? {
            let path = entry.map_err(io_error(&at))?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
            if files.len() > SNAPSHOT_FILES_MAX {
                return Err(Error::SavedAnchors(format!(
                    "{} holds over {SNAPSHOT_FILES_MAX} files: it isn't a snapshot",
                    dir.display()
                )));
            }
        }
    }
    files.sort();
    let mut parts: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len());
    let mut bytes_total: u64 = 0;
    for path in &files {
        let bytes = read_bounded(path, SNAPSHOT_BYTES_MAX - bytes_total)?;
        bytes_total += bytes.len() as u64;
        let relative = path.strip_prefix(dir).unwrap_or(path);
        parts.push((relative.display().to_string(), bytes));
    }
    assert!(bytes_total <= SNAPSHOT_BYTES_MAX);
    Ok(crate::versions::hash_parts(
        parts
            .iter()
            .map(|(label, bytes)| (label.as_str(), bytes.as_slice())),
    ))
}

fn file_sha256(path: &Path, bytes_max: u64) -> Result<String> {
    let bytes = read_bounded(path, bytes_max)?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The file at `path`, refused when it holds more than `bytes_max`.
fn read_bounded(path: &Path, bytes_max: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(io_error(path))?;
    let mut bytes = Vec::new();
    file.take(bytes_max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error(path))?;
    if bytes.len() as u64 > bytes_max {
        return Err(Error::SavedAnchors(format!(
            "{} is over its {bytes_max}-byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grade::tests::{scenario, session};

    fn identity() -> Identity {
        Identity {
            grader_version: crate::versions::GRADER_VERSION,
            grader_sha256: "g".repeat(64),
            eval_seed_sha256: "e".repeat(64),
            scenarios: vec!["a".into(), "b".into()],
            draw_sha256: "d".repeat(64),
            snapshot_sha256: "s".repeat(64),
            reference_sha256: "r".repeat(64),
            informed_sha256: Some("i".repeat(64)),
            v2_sessions: false,
        }
    }

    fn named(name: &str, pnl_usd: f64) -> SessionReport {
        let mut report = session(pnl_usd);
        report.scenario = name.into();
        report.max_drawdown_usd = pnl_usd.abs() / 3.0;
        report.timeline.start_slot = 7;
        report.timeline.slots_per_step = 2;
        report.timeline.steps = (0..5)
            .map(|k| {
                // Marks that a decimal rounding would change.
                let mark_usd = 100.0 + (k as f64) / 3.0;
                crate::session::StepRecord {
                    k,
                    at: k * 2,
                    mark_usd,
                    index_usd: mark_usd,
                    level_usd: mark_usd,
                    book_usd: None,
                    fair_usd: None,
                    best_bid_usd: None,
                    best_ask_usd: None,
                    impact_bps: None,
                    trades: None,
                    position: 0.0,
                    value_usd: 10_100.0,
                    pnl_usd: 0.0,
                    hold_usd: 0.0,
                    turn_ms: None,
                    answered: None,
                    markets: Vec::new(),
                }
            })
            .collect();
        report
    }

    fn saved() -> SavedAnchors {
        SavedAnchors {
            format: FORMAT,
            identity: identity(),
            reference: ["a", "b"]
                .iter()
                .zip([12.345_678_9, -0.1])
                .map(|(name, pnl)| SavedSession::of(&named(name, pnl)))
                .collect(),
            informed: Some(
                ["a", "b"]
                    .iter()
                    .zip([40.0 / 3.0, 7.0])
                    .map(|(name, pnl)| SavedSession::of(&named(name, pnl)))
                    .collect(),
            ),
            nops: vec![None, Some(1.0 / 7.0)],
        }
    }

    fn refusal(this: &Identity, saved: &Identity) -> String {
        this.check(saved).unwrap_err().to_string()
    }

    #[test]
    fn the_same_identity_is_accepted() {
        assert!(identity().check(&identity()).is_ok());
    }

    #[test]
    fn a_grade_of_another_set_is_refused() {
        let mut other = identity();
        other.eval_seed_sha256 = "f".repeat(64);
        assert!(refusal(&other, &identity()).contains("secret"));
    }

    #[test]
    fn other_scenarios_or_another_order_are_refused() {
        let mut fewer = identity();
        fewer.scenarios.pop();
        assert!(refusal(&fewer, &identity()).contains("size"));
        let mut swapped = identity();
        swapped.scenarios.swap(0, 1);
        let why = refusal(&swapped, &identity());
        assert!(why.contains("session 1's scenario (b, saved a)"), "{why}");
        let mut redrawn = identity();
        redrawn.draw_sha256 = "x".repeat(64);
        assert!(refusal(&redrawn, &identity()).contains("as drawn"));
    }

    #[test]
    fn other_anchors_are_refused() {
        let mut reference = identity();
        reference.reference_sha256 = "x".repeat(64);
        assert!(refusal(&reference, &identity()).contains("reference's component"));
        let mut informed = identity();
        informed.informed_sha256 = Some("x".repeat(64));
        assert!(refusal(&informed, &identity()).contains("informed bot's component"));
        let mut none = identity();
        none.informed_sha256 = None;
        assert!(refusal(&none, &identity()).contains("one was saved"));
        assert!(refusal(&identity(), &none).contains("none was saved"));
    }

    #[test]
    fn another_grader_snapshot_or_session_rules_are_refused() {
        let mut grader = identity();
        grader.grader_sha256 = "x".repeat(64);
        assert!(refusal(&grader, &identity()).contains("the grader"));
        let mut snapshot = identity();
        snapshot.snapshot_sha256 = "x".repeat(64);
        assert!(refusal(&snapshot, &identity()).contains("snapshot"));
        let mut rules = identity();
        rules.v2_sessions = true;
        assert!(refusal(&rules, &identity()).contains("--v2-sessions"));
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mtx-saved-anchors-{name}-{}", uuid::Uuid::new_v4()))
    }

    /// What a reusing grade reads back is what the saving grade had, byte for
    /// byte once serialized, the steps' marks included: its reports and its
    /// v2 hashes come out the same.
    #[test]
    fn saved_sessions_read_back_exactly() {
        let original = saved();
        let path = temp_path("exact");
        original.save(&path).unwrap();
        let loaded = SavedAnchors::load(&path, &identity()).unwrap();
        std::fs::remove_file(&path).unwrap();
        let fresh = original.into_sessions();
        for (a, b) in fresh.reference.iter().zip(&loaded.reference) {
            assert_eq!(
                serde_json::to_string(a).unwrap(),
                serde_json::to_string(b).unwrap()
            );
            assert_eq!(a.timeline.steps, b.timeline.steps);
            assert_eq!(
                crate::grade::reference_sha256(a),
                crate::grade::reference_sha256(b)
            );
        }
        let (fresh_informed, loaded_informed) = (fresh.informed.unwrap(), loaded.informed.unwrap());
        for (a, b) in fresh_informed.iter().zip(&loaded_informed) {
            assert_eq!(a.pnl_usd.to_bits(), b.pnl_usd.to_bits());
            assert_eq!(a.max_drawdown_usd.to_bits(), b.max_drawdown_usd.to_bits());
        }
        assert_eq!(fresh.nops, loaded.nops);
    }

    #[test]
    fn a_file_for_another_grade_or_format_is_refused() {
        let path = temp_path("refused");
        saved().save(&path).unwrap();
        let mut other = identity();
        other.eval_seed_sha256 = "f".repeat(64);
        assert!(SavedAnchors::load(&path, &other).is_err());
        let mut old = saved();
        old.format = FORMAT + 1;
        old.save(&path).unwrap();
        let why = SavedAnchors::load(&path, &identity())
            .unwrap_err()
            .to_string();
        assert!(why.contains("format"), "{why}");
        std::fs::write(&path, b"{}").unwrap();
        let why = SavedAnchors::load(&path, &identity())
            .unwrap_err()
            .to_string();
        assert!(why.contains("isn't a saved anchors file"), "{why}");
        std::fs::remove_file(&path).unwrap();
        assert!(SavedAnchors::load(&path, &identity()).is_err());
    }

    #[test]
    fn sessions_out_of_order_or_missing_are_refused() {
        let mut swapped = saved();
        swapped.reference.swap(0, 1);
        assert!(swapped.check_shape().is_err());
        let mut short = saved();
        short.informed.as_mut().unwrap().pop();
        assert!(short.check_shape().is_err());
        let mut unsaved = saved();
        unsaved.informed = None;
        assert!(unsaved.check_shape().is_err());
        let mut nops = saved();
        nops.nops.pop();
        assert!(nops.check_shape().is_err());
        assert!(saved().check_shape().is_ok());
    }

    #[test]
    fn the_draw_hash_covers_the_scenarios_and_the_latent_state() {
        let scenarios = vec![scenario(None), scenario(None)];
        let latent = vec!["{}".to_string(), "{}".to_string()];
        let base = draw_sha256(&scenarios, Some(&latent)).unwrap();
        assert_eq!(base, draw_sha256(&scenarios, Some(&latent)).unwrap());
        let mut moved = scenarios.clone();
        moved[1].duration_slots += 1;
        assert_ne!(base, draw_sha256(&moved, Some(&latent)).unwrap());
        let told = vec!["{}".to_string(), "{\"informed\":1}".to_string()];
        assert_ne!(base, draw_sha256(&scenarios, Some(&told)).unwrap());
        assert_ne!(base, draw_sha256(&scenarios, None).unwrap());
    }

    #[test]
    fn a_process_anchor_and_an_oversized_file_are_refused() {
        let dir = temp_path("process");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Justfile"), "run:\n\ttrue\n").unwrap();
        assert!(
            component_sha256(&dir)
                .unwrap_err()
                .to_string()
                .contains("bot.wasm")
        );
        std::fs::write(dir.join("bot.wasm"), b"wasm").unwrap();
        assert_eq!(component_sha256(&dir).unwrap().len(), 64);
        assert!(read_bounded(&dir.join("bot.wasm"), 3).is_err());
        assert_eq!(read_bounded(&dir.join("bot.wasm"), 4).unwrap(), b"wasm");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
