//! Signing.
//!
//! [`TransactionSigner`] is the one thing plan execution needs from a key:
//! its public key, and a signature over a transaction message. Anything that
//! can do that (an in-memory keypair, a hardware wallet, a remote KMS, an OS
//! keychain behind biometrics) can sign a plan unchanged.
//!
//! The default backend is [`Keypair`] itself, loaded from a Solana CLI
//! keypair file with [`read_keypair_file`]. Keychain-backed signers (Touch
//! ID) are deliberately not part of this crate: they pull platform
//! dependencies that the sandboxes this crate runs in do not have.
//!
//! Signing is async because remote backends are; [`Keypair`] completes
//! immediately.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{Signature, versioned::VersionedTransaction};

use crate::chain::error::SignerError;

/// Produces signatures for transactions.
#[async_trait]
pub trait TransactionSigner: Send + Sync {
    /// The public key signatures verify against.
    fn address(&self) -> Pubkey;

    /// Signs `message` bytes.
    async fn sign_message(&self, message: &[u8]) -> Result<Signature, SignerError>;

    /// Adds this signer's signature to `tx`, in the slot its public key
    /// occupies among the required signers. Fails if it is not a required
    /// signer. Other signers' slots are left untouched.
    async fn sign_transaction(&self, tx: &mut VersionedTransaction) -> Result<(), SignerError> {
        let pubkey = self.address();
        let required = usize::from(tx.message.header().num_required_signatures);
        let index = tx
            .message
            .static_account_keys()
            .iter()
            .take(required)
            .position(|key| *key == pubkey)
            .ok_or(SignerError::NotASigner(pubkey))?;
        let signature = self.sign_message(&tx.message.serialize()).await?;
        if tx.signatures.len() != required {
            tx.signatures.resize(required, Signature::default());
        }
        tx.signatures[index] = signature;
        Ok(())
    }
}

#[async_trait]
impl TransactionSigner for Keypair {
    fn address(&self) -> Pubkey {
        Signer::pubkey(self)
    }

    async fn sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        Signer::try_sign_message(self, message).map_err(|e| SignerError::Backend(e.to_string()))
    }
}

#[async_trait]
impl<T: TransactionSigner + ?Sized> TransactionSigner for Arc<T> {
    fn address(&self) -> Pubkey {
        (**self).address()
    }

    async fn sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        (**self).sign_message(message).await
    }

    async fn sign_transaction(&self, tx: &mut VersionedTransaction) -> Result<(), SignerError> {
        (**self).sign_transaction(tx).await
    }
}

#[async_trait]
impl<T: TransactionSigner + ?Sized> TransactionSigner for &T {
    fn address(&self) -> Pubkey {
        (**self).address()
    }

    async fn sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        (**self).sign_message(message).await
    }

    async fn sign_transaction(&self, tx: &mut VersionedTransaction) -> Result<(), SignerError> {
        (**self).sign_transaction(tx).await
    }
}

/// `true` when every required signature slot holds a non-default signature.
pub fn is_fully_signed(tx: &VersionedTransaction) -> bool {
    let required = usize::from(tx.message.header().num_required_signatures);
    tx.signatures.len() == required && tx.signatures.iter().all(|s| *s != Signature::default())
}

/// Reads a Solana CLI keypair file: a JSON array of the 64 secret-key bytes.
pub fn read_keypair_file(path: impl AsRef<Path>) -> Result<Keypair, SignerError> {
    let path = path.as_ref();
    let file_error = |detail: String| SignerError::KeypairFile {
        path: path.display().to_string(),
        detail,
    };
    let contents = std::fs::read_to_string(path).map_err(|e| file_error(e.to_string()))?;
    let bytes: Vec<u8> = parse_byte_array(&contents)
        .ok_or_else(|| file_error("not a JSON array of bytes".into()))?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| SignerError::InvalidKeypair(e.to_string()))
}

/// Parses `[1, 2, 3]` without a JSON dependency.
fn parse_byte_array(text: &str) -> Option<Vec<u8>> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|n| n.trim().parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use solana_system_interface::instruction as system_instruction;
    use solana_transaction::{Message, Transaction};

    use super::*;

    fn transfer_tx(payer: &Pubkey) -> VersionedTransaction {
        let ix = system_instruction::transfer(payer, &Pubkey::new_unique(), 1);
        VersionedTransaction::from(Transaction::new_unsigned(Message::new(&[ix], Some(payer))))
    }

    #[tokio::test]
    async fn keypair_signs_its_slot() {
        let keypair = Keypair::new();
        let mut tx = transfer_tx(&keypair.address());
        assert!(!is_fully_signed(&tx));
        keypair.sign_transaction(&mut tx).await.unwrap();
        assert!(is_fully_signed(&tx));
        let message = tx.message.serialize();
        assert!(tx.signatures[0].verify(keypair.address().as_ref(), &message));
    }

    #[tokio::test]
    async fn refuses_to_sign_for_others() {
        let keypair = Keypair::new();
        let mut tx = transfer_tx(&Pubkey::new_unique());
        let err = keypair.sign_transaction(&mut tx).await.unwrap_err();
        assert_eq!(err, SignerError::NotASigner(keypair.address()));
    }

    #[tokio::test]
    async fn arc_and_ref_delegate() {
        async fn sign_with(signer: &dyn TransactionSigner, tx: &mut VersionedTransaction) {
            signer.sign_transaction(tx).await.unwrap();
        }
        let keypair = Arc::new(Keypair::new());
        let mut tx = transfer_tx(&keypair.address());
        sign_with(&keypair, &mut tx).await;
        assert!(is_fully_signed(&tx));
        let mut tx = transfer_tx(&keypair.address());
        sign_with(&&*keypair, &mut tx).await;
        assert!(is_fully_signed(&tx));
    }

    #[test]
    fn keypair_file_round_trip() {
        let keypair = Keypair::new();
        let dir = std::env::temp_dir().join(format!("luce-signer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("id.json");
        let bytes: Vec<String> = keypair.to_bytes().iter().map(u8::to_string).collect();
        std::fs::write(&path, format!("[{}]", bytes.join(","))).unwrap();
        let loaded = read_keypair_file(&path).unwrap();
        assert_eq!(Signer::pubkey(&loaded), Signer::pubkey(&keypair));

        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(
            read_keypair_file(&path).unwrap_err(),
            SignerError::KeypairFile { .. }
        ));
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        assert!(matches!(
            read_keypair_file(&path).unwrap_err(),
            SignerError::InvalidKeypair(_)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
