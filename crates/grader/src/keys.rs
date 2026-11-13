//! The keypairs a grade's sessions run with: the bot's own, and the grader's
//! takers and maker.
//!
//! Each is derived from a secret only the grader holds and the session's
//! name, so the same grade signs the same transactions every run: same
//! signatures, same accounts, same work for a bot that reads them (see
//! [`crate::replay`]). Nobody without the secret can derive them, so a bot,
//! which knows its own key and every public value, can't sign as the
//! grader's traders. The secret is never written anywhere: not to reports,
//! logs or files.
//!
//! A drawn hidden set derives the secret from the batch's eval seed, so
//! every grade of a batch, on any machine, uses the same keys. A grade of
//! scenario files has no seed: it draws a fresh secret, so its runs
//! (`mtx grade --replay`) agree with each other, not with another grade.

use sha2::{Digest, Sha256};
use solana_keypair::Keypair;

/// Domain separation: these hashes are keys, and nothing else derived from
/// the eval seed (the draw, the reported hash) can be one of them.
const SECRET_DOMAIN: &[u8] = b"mtx-grader/session-keys/v1/secret";
const KEY_DOMAIN: &[u8] = b"mtx-grader/session-keys/v1/key";
const SEED_DOMAIN: &[u8] = b"mtx-grader/session-keys/v1/seed";

/// The secret a grade's session keys come from.
#[derive(Clone)]
pub struct SessionKeys {
    secret: [u8; 32],
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKeys(..)")
    }
}

impl SessionKeys {
    /// Keys for a drawn hidden set, from the batch's secret (`MTX_EVAL_SEED`).
    pub fn from_eval_seed(seed: &[u8]) -> Self {
        Self {
            secret: digest(&[SECRET_DOMAIN, seed]),
        }
    }

    /// Keys from a fresh secret: for a grade with no eval seed.
    pub fn random() -> Self {
        // Two v4 UUIDs: 244 bits from the OS's randomness.
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        Self {
            secret: digest(&[SECRET_DOMAIN, a.as_bytes(), b.as_bytes()]),
        }
    }

    /// One session's keys. `session` names it among the grade's sessions
    /// (its role and scenario), so no two share a key.
    pub fn session(&self, session: &str) -> SessionKeyring<'_> {
        SessionKeyring {
            keys: self,
            session: session.to_string(),
        }
    }

    /// The keys of one side of the grade: `bot` or `reference`.
    pub fn role(&self, role: &'static str) -> RoleKeys<'_> {
        RoleKeys { keys: self, role }
    }
}

/// One side of a grade's sessions: see [`SessionKeys::role`].
#[derive(Clone, Copy)]
pub struct RoleKeys<'a> {
    keys: &'a SessionKeys,
    role: &'static str,
}

impl<'a> RoleKeys<'a> {
    /// The keys of the `index`th session, on `scenario`.
    pub fn session(&self, index: usize, scenario: &str) -> SessionKeyring<'a> {
        self.keys
            .session(&format!("{}/{index}/{scenario}", self.role))
    }
}

/// One session's keys: see [`SessionKeys::session`].
pub struct SessionKeyring<'a> {
    keys: &'a SessionKeys,
    session: String,
}

impl SessionKeyring<'_> {
    /// The keypair for `role` (`bot`, `taker`, `competitor`), the `index`th
    /// of its kind.
    pub fn keypair(&self, role: &str, index: u32) -> Keypair {
        Keypair::new_from_array(self.derive(KEY_DOMAIN, role, index))
    }

    /// The seed of `role`'s randomness, the `index`th of its kind: the
    /// grader's actors draw from it, so a session replays the same draws.
    /// No key derives from it, and it is never written anywhere.
    pub fn seed(&self, role: &str, index: u32) -> [u8; 32] {
        self.derive(SEED_DOMAIN, role, index)
    }

    fn derive(&self, domain: &[u8], role: &str, index: u32) -> [u8; 32] {
        let field = |bytes: &[u8]| {
            let mut framed = (bytes.len() as u64).to_le_bytes().to_vec();
            framed.extend_from_slice(bytes);
            framed
        };
        digest(&[
            domain,
            &self.keys.secret,
            &field(self.session.as_bytes()),
            &field(role.as_bytes()),
            &index.to_le_bytes(),
        ])
    }
}

fn digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
    }
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use solana_signer::Signer;

    use super::*;

    #[test]
    fn keys_replay_and_never_repeat() {
        let keys = SessionKeys::from_eval_seed(b"a batch secret of at least 32 chars");
        let again = SessionKeys::from_eval_seed(b"a batch secret of at least 32 chars");
        let bot = keys.session("bot/0/crash-1").keypair("bot", 0);
        assert_eq!(
            bot.pubkey(),
            again.session("bot/0/crash-1").keypair("bot", 0).pubkey()
        );
        let others = [
            keys.session("bot/0/crash-1").keypair("taker", 0),
            keys.session("bot/0/crash-1").keypair("bot", 1),
            keys.session("bot/1/crash-1").keypair("bot", 0),
            keys.session("reference/0/crash-1").keypair("bot", 0),
            SessionKeys::from_eval_seed(b"another batch secret, 32 chars long")
                .session("bot/0/crash-1")
                .keypair("bot", 0),
        ];
        for other in &others {
            assert_ne!(bot.pubkey(), other.pubkey());
        }
        // A seed is no key, and replays like one.
        let ring = keys.session("bot/0/crash-1");
        assert_eq!(
            ring.seed("noise", 0),
            again.session("bot/0/crash-1").seed("noise", 0)
        );
        assert_ne!(ring.seed("noise", 0), ring.seed("noise", 1));
        assert_ne!(&ring.seed("bot", 0), bot.secret_bytes());
        // Fields are framed: moving a byte between them changes the key.
        assert_ne!(
            keys.session("ab").keypair("c", 0).pubkey(),
            keys.session("a").keypair("bc", 0).pubkey()
        );
    }

    #[test]
    fn the_secret_never_prints() {
        let keys = SessionKeys::from_eval_seed(b"a batch secret of at least 32 chars");
        assert_eq!(format!("{keys:?}"), "SessionKeys(..)");
        assert_ne!(
            SessionKeys::random()
                .session("s")
                .keypair("bot", 0)
                .pubkey(),
            SessionKeys::random()
                .session("s")
                .keypair("bot", 0)
                .pubkey()
        );
    }

    /// After a grade of a drawn set, none of its session keys' secrets is
    /// anywhere in what the grade wrote: its report, details or logs. Run
    /// with the grade's `MTX_EVAL_SEED` and `MTX_TEST_GRADE_DIR` (searched
    /// recursively, `.gz` files unpacked); `report.json` there names the
    /// sessions.
    #[test]
    #[ignore = "needs a finished grade: MTX_TEST_GRADE_DIR and its MTX_EVAL_SEED"]
    fn no_session_key_appears_in_a_grades_output() {
        use std::io::Read;
        let dir = std::path::PathBuf::from(std::env::var("MTX_TEST_GRADE_DIR").unwrap());
        let seed = std::env::var("MTX_EVAL_SEED").unwrap();
        let keys = SessionKeys::from_eval_seed(seed.as_bytes());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("report.json")).unwrap()).unwrap();
        let scenarios: Vec<String> = report["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["scenario"].as_str().unwrap().to_string())
            .collect();
        assert!(!scenarios.is_empty());
        let mut secrets = vec![seed.clone(), hex(&keys.secret)];
        for (index, scenario) in scenarios.iter().enumerate() {
            for role in ["bot", "reference"] {
                let keyring = keys.role(role).session(index, scenario);
                let mut pairs = vec![keyring.keypair("bot", 0), keyring.keypair("competitor", 0)];
                pairs.extend(
                    (0..crate::making::TAKER_ACCOUNTS as u32).map(|i| keyring.keypair("taker", i)),
                );
                for pair in pairs {
                    secrets.push(pair.to_base58_string());
                    secrets.push(hex(pair.secret_bytes()));
                    secrets.push(format!("{:?}", &pair.to_bytes()[..]).replace(' ', ""));
                }
            }
        }
        let mut files = 0;
        let mut stack = vec![dir];
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            let mut bytes = std::fs::read(&path).unwrap();
            if path.extension().is_some_and(|e| e == "gz") {
                let mut out = Vec::new();
                flate2::read::GzDecoder::new(&bytes[..])
                    .read_to_end(&mut out)
                    .unwrap();
                bytes = out;
            }
            let text = String::from_utf8_lossy(&bytes).replace([' ', '\n'], "");
            for secret in &secrets {
                assert!(
                    !text.contains(secret.as_str()),
                    "a session key in {}",
                    path.display()
                );
            }
            files += 1;
        }
        assert!(files > 1, "nothing to search");
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
