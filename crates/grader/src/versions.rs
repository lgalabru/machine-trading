//! Which version of a task and of the scoring rule produced a grade.
//!
//! Every report carries a [`Versions`]: the task's fingerprint (a hash over
//! everything that changes what the task measures, computed by `mtx task
//! fingerprint` and shipped to the verifier as `tests/task.fingerprint`) and
//! the scoring spec's hash ([`crate::scoring_v2::spec_hash`]). Two grades
//! are comparable only when both match.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The grader's own version: bump it only when a session's meaning changes in
/// a way the anchors' digest (`tests/anchor.digest`) doesn't show (a guard's
/// rule, a disqualification, a cost only a bot meets). Not when a digest
/// differs: the digest is in that task's fingerprint, so `mtx task sync
/// --anchors` moves that task's fingerprint alone, and this moves every
/// task's.
pub const GRADER_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    /// `mtx task fingerprint`'s hash of the task the grade ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_fingerprint: Option<String>,
    /// [`crate::scoring_v2::spec_hash`] of the settings that scored it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring_hash: Option<String>,
}

impl Versions {
    pub fn is_empty(&self) -> bool {
        self.task_fingerprint.is_none() && self.scoring_hash.is_none()
    }
}

/// SHA-256 of labelled parts, hex. Each part is length-prefixed, so
/// `("ab", "c")` and `("a", "bc")` differ.
pub fn hash_parts<'a>(parts: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut hasher = Sha256::new();
    for (label, bytes) in parts {
        for field in [label.as_bytes(), bytes] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field);
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_are_delimited() {
        assert_ne!(
            hash_parts([("a", b"bc".as_slice())]),
            hash_parts([("ab", b"c".as_slice())])
        );
        assert_eq!(hash_parts([("a", b"b".as_slice())]).len(), 64);
    }

    #[test]
    fn empty_versions_serialize_to_nothing() {
        assert_eq!(serde_json::to_string(&Versions::default()).unwrap(), "{}");
    }
}
