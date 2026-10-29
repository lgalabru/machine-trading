//! Accounts: the wallets a user controls.
//!
//! A user may hold several wallets (a main account, a test account, one per
//! strategy family). Every strategy belongs to exactly one account, and every
//! transaction is signed by that account's key. This module records *which*
//! wallets exist and *how* their keys are kept; it never touches key material.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Unique identifier for an account record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub Uuid);

impl AccountId {
    /// Generates a fresh random identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Errors from account validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AccountError {
    /// Names must be 1–32 characters from `[A-Za-z0-9._-]`.
    #[error("invalid account name {0:?}: use 1-32 characters from a-z, A-Z, 0-9, '.', '_', '-'")]
    InvalidName(String),
}

/// A validated, human-chosen account name such as `"main"` or `"test-1"`.
///
/// The character set matches what secure keystores accept as an entry name,
/// so a name that is valid here is valid everywhere it will be used.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountName(String);

impl AccountName {
    /// Maximum length in characters.
    pub const MAX_LEN: usize = 32;

    /// Validates and wraps a name.
    pub fn new(name: impl Into<String>) -> Result<Self, AccountError> {
        let name = name.into();
        let valid_len = !name.is_empty() && name.len() <= Self::MAX_LEN;
        let valid_chars = name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if valid_len && valid_chars {
            Ok(Self(name))
        } else {
            Err(AccountError::InvalidName(name))
        }
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccountName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AccountName {
    type Error = AccountError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<AccountName> for String {
    fn from(value: AccountName) -> Self {
        value.0
    }
}

/// Where an account's private key lives and what guards access to it.
///
/// This is a description, not a handle: the runtime crate turns it into a
/// live signer. Keeping it as plain data lets the database and the CLI reason
/// about accounts without linking any key-storage backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignerBackend {
    /// Platform keychain gated by the OS (Touch ID on macOS).
    PlatformKeychain,
    /// An owner-only JSON keypair file on disk (Solana CLI format).
    File {
        /// Absolute path to the keypair file.
        path: String,
    },
}

impl std::fmt::Display for SignerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PlatformKeychain => f.write_str("platform keychain"),
            Self::File { path } => write!(f, "file {path}"),
        }
    }
}

/// A wallet the user controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Stable identifier.
    pub id: AccountId,
    /// Human-chosen name, unique per user.
    pub name: AccountName,
    /// Public address (base58 public key on Solana).
    pub address: String,
    /// Where the private key is kept.
    pub signer: SignerBackend,
    /// When the account was registered.
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for name in ["main", "test-1", "ludo.2", "A_b", &"x".repeat(32)] {
            assert!(AccountName::new(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn invalid_names() {
        for name in [
            "",
            "with space",
            "slash/",
            "ünïcode",
            &"x".repeat(33),
            "a:b",
        ] {
            assert_eq!(
                AccountName::new(name).unwrap_err(),
                AccountError::InvalidName(name.to_owned()),
                "{name}"
            );
        }
    }

    #[test]
    fn name_serde_round_trip_validates() {
        let name: AccountName = serde_json::from_str("\"main\"").unwrap();
        assert_eq!(name.as_str(), "main");
        assert!(serde_json::from_str::<AccountName>("\"bad name\"").is_err());
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"main\"");
    }

    #[test]
    fn signer_backend_serde() {
        let json = serde_json::to_string(&SignerBackend::PlatformKeychain).unwrap();
        assert_eq!(json, r#"{"kind":"platform_keychain"}"#);
        let file = SignerBackend::File {
            path: "/tmp/k.json".into(),
        };
        let json = serde_json::to_string(&file).unwrap();
        assert_eq!(json, r#"{"kind":"file","path":"/tmp/k.json"}"#);
        assert_eq!(serde_json::from_str::<SignerBackend>(&json).unwrap(), file);
    }
}
