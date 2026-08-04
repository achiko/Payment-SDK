//! Protocol-independent credential signing contracts.
//!
//! Concrete credentials live in adapter crates. Transaction signing, network
//! addresses, and wallet routing intentionally live above this crate.

use std::{error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest(String);

impl Digest {
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
pub struct PublicKey(String);

impl PublicKey {
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
pub struct CredentialSignature(String);

impl CredentialSignature {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerError {
    GenerationFailed,
    SigningFailed,
}

impl fmt::Display for SignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GenerationFailed => formatter.write_str("mock credential generation failed"),
            Self::SigningFailed => formatter.write_str("mock credential signing failed"),
        }
    }
}

impl Error for SignerError {}

/// Represents one credential capable of signing a protocol-independent digest.
pub trait Signer {
    type Signature;
    type Error;

    fn public_key(&self) -> &PublicKey;

    fn sign_digest(&self, digest: &Digest) -> Result<Self::Signature, Self::Error>;

    fn sign_message(&self, message: &str) -> Result<Self::Signature, Self::Error> {
        self.sign_digest(&Digest::new(message))
    }
}
