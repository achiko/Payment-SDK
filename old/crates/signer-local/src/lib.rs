//! Mock locally owned signing credential.
//!
//! `LocalSigner::generate` uses a process-local counter, not cryptographic
//! randomness. It exists only to demonstrate credential ownership and wiring.

use std::sync::atomic::{AtomicU64, Ordering};

use signer::{CredentialSignature, Digest, PublicKey, Signer, SignerError};

static NEXT_CREDENTIAL_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialId(String);

impl CredentialId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSigner {
    credential_id: CredentialId,
    public_key: PublicKey,
}

impl LocalSigner {
    /// Generates a unique mock credential for this process.
    ///
    /// This method does not create a real private key and is not secure.
    pub fn generate() -> Result<Self, SignerError> {
        let id = NEXT_CREDENTIAL_ID.fetch_add(1, Ordering::Relaxed);

        Ok(Self {
            credential_id: CredentialId::new(format!("mock-credential-{id}")),
            public_key: PublicKey::new(format!("mock-public-key-{id}")),
        })
    }

    #[must_use]
    pub fn from_parts(credential_id: CredentialId, public_key: PublicKey) -> Self {
        Self {
            credential_id,
            public_key,
        }
    }

    #[must_use]
    pub const fn credential_id(&self) -> &CredentialId {
        &self.credential_id
    }
}

impl Signer for LocalSigner {
    type Signature = CredentialSignature;
    type Error = SignerError;

    fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    fn sign_digest(&self, _digest: &Digest) -> Result<Self::Signature, Self::Error> {
        Ok(CredentialSignature::new("mock local credential signature"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_mock_credentials() -> Result<(), SignerError> {
        let first = LocalSigner::generate()?;
        let second = LocalSigner::generate()?;

        assert_ne!(first.credential_id(), second.credential_id());
        assert_ne!(first.public_key(), second.public_key());
        Ok(())
    }

    #[test]
    fn signs_a_mock_digest() -> Result<(), SignerError> {
        let signer = LocalSigner::generate()?;
        let signature = signer.sign_digest(&Digest::new("mock digest"))?;

        assert_eq!(signature.as_str(), "mock local credential signature");
        Ok(())
    }
}
