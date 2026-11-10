//! Error types for the Solana runtime.

use solana_pubkey::Pubkey;
use solana_transaction::{InstructionError, Signature, TransactionError};
use thiserror::Error;

/// Errors from signing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SignerError {
    /// The signer is not one of the transaction's required signers.
    #[error("{0} is not a required signer of this transaction")]
    NotASigner(Pubkey),
    /// The transaction still lacks signatures after signing.
    #[error("transaction is missing {missing} of {required} signatures")]
    Incomplete { missing: usize, required: usize },
    /// Key bytes were not a valid Ed25519 keypair.
    #[error("invalid keypair: {0}")]
    InvalidKeypair(String),
    /// Reading a keypair file failed.
    #[error("keypair file {path}: {detail}")]
    KeypairFile { path: String, detail: String },
    /// A signing backend failed or refused.
    #[error("signing failed: {0}")]
    Backend(String),
}

/// Errors from packing instructions into transactions.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    /// An atomic group does not fit in one transaction on its own.
    #[error("instruction group {label:?} serializes to {size} bytes; the limit is {limit}")]
    GroupTooLarge {
        label: String,
        size: usize,
        limit: usize,
    },
    /// Nothing to do.
    #[error("plan has no instructions")]
    Empty,
}

/// Errors from talking to the chain or executing a plan.
#[derive(Debug, Error)]
pub enum ChainError {
    /// The RPC node returned an error or was unreachable.
    #[error("rpc: {0}")]
    Rpc(String),
    /// An account the operation needs does not exist.
    #[error("account {0} not found")]
    AccountNotFound(Pubkey),
    /// Account data did not have the expected shape.
    #[error("decode {what}: {detail}")]
    Decode { what: &'static str, detail: String },
    /// Pre-flight simulation of a step failed; nothing was signed or sent.
    #[error("simulation of {label:?} failed: {error}\n{}", logs.join("\n"))]
    Simulation {
        label: String,
        error: TransactionError,
        logs: Vec<String>,
    },
    /// The transaction was sent but failed on-chain.
    #[error("transaction {signature} ({label:?}) failed on-chain: {error}")]
    Failed {
        label: String,
        signature: Signature,
        error: TransactionError,
    },
    /// The signer does not control the plan's fee payer.
    #[error("signer {actual} cannot pay for a plan whose payer is {expected}")]
    PayerMismatch { expected: Pubkey, actual: Pubkey },
    /// Signing failed.
    #[error(transparent)]
    Signer(#[from] SignerError),
    /// Packing failed.
    #[error(transparent)]
    Plan(#[from] PlanError),
}

impl From<solana_rpc_client_api::client_error::Error> for ChainError {
    fn from(err: solana_rpc_client_api::client_error::Error) -> Self {
        Self::Rpc(err.to_string())
    }
}

impl ChainError {
    /// The custom program error code carried by a simulation or on-chain
    /// failure, if the failure was an instruction's `Custom(code)`: the
    /// instruction index and the code.
    ///
    /// Protocol adapters map these codes to their own error vocabulary.
    pub fn custom_error_code(&self) -> Option<(u8, u32)> {
        let error = match self {
            Self::Simulation { error, .. } | Self::Failed { error, .. } => error,
            _ => return None,
        };
        match error {
            TransactionError::InstructionError(index, InstructionError::Custom(code)) => {
                Some((*index, *code))
            }
            _ => None,
        }
    }

    /// The last `n` program log lines of a failed simulation, joined by
    /// newlines; empty for other errors.
    pub fn log_tail(&self, n: usize) -> String {
        match self {
            Self::Simulation { logs, .. } => logs[logs.len().saturating_sub(n)..].join("\n"),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_error_code_extraction() {
        let err = ChainError::Simulation {
            label: "deposit".into(),
            error: TransactionError::InstructionError(2, InstructionError::Custom(6008)),
            logs: vec![],
        };
        assert_eq!(err.custom_error_code(), Some((2, 6008)));

        let err = ChainError::Failed {
            label: "deposit".into(),
            signature: Signature::default(),
            error: TransactionError::InstructionError(0, InstructionError::InvalidArgument),
        };
        assert_eq!(err.custom_error_code(), None);

        assert_eq!(ChainError::Rpc("x".into()).custom_error_code(), None);
    }

    #[test]
    fn simulation_error_renders_logs() {
        let err = ChainError::Simulation {
            label: "deposit".into(),
            error: TransactionError::AccountNotFound,
            logs: vec!["Program log: a".into(), "Program log: b".into()],
        };
        let text = err.to_string();
        assert!(text.contains("\"deposit\""));
        assert!(text.contains("Program log: a\nProgram log: b"));
        assert_eq!(err.log_tail(1), "Program log: b");
        assert_eq!(err.log_tail(5), "Program log: a\nProgram log: b");
    }
}
