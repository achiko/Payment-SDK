use crate::{KeyLocator, SignatureEncoding, SignatureScheme, SignerError, SignerErrorKind};
use std::{fmt, str::FromStr};

/// Opaque durable identity assigned by the caller to one custody operation.
///
/// Replaying the same ID with identical request content must return the same
/// remote operation result. Reusing it with different content is a conflict.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(String);

impl OperationId {
    pub fn new(value: impl Into<String>) -> Result<Self, SignerError> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 {
            return Err(SignerError {
                kind: SignerErrorKind::InvalidRequest,
                message: "signing operation ID must contain between 1 and 256 bytes".to_owned(),
            });
        }
        if value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(SignerError {
                kind: SignerErrorKind::InvalidRequest,
                message: "signing operation ID must not contain whitespace or control characters"
                    .to_owned(),
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn child(&self, suffix: impl AsRef<str>) -> Result<Self, SignerError> {
        let suffix = suffix.as_ref();
        if suffix.trim().is_empty() {
            return Err(SignerError {
                kind: SignerErrorKind::InvalidRequest,
                message: "signing operation ID suffix must not be empty".to_owned(),
            });
        }
        Self::new(format!("{}:{suffix}", self.0))
    }
}

impl FromStr for OperationId {
    type Err = SignerError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Debug for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OperationId([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    /// The chain or protocol computes this digest. The signer does not hash it again.
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignablePayload {
    Message(Vec<u8>),
    Digest(Digest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserInteraction {
    NotRequired,
    Allowed,
    Required,
}

/// Optional curve-level key transformation applied inside the custody boundary
/// before signing. The caller provides only public tweak material; private key
/// bytes never leave the signer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyTweak {
    /// Adds a BIP340-compatible secp256k1 scalar to the signing key. This is
    /// used by Bitcoin Taproot key-path spends.
    Secp256k1Add([u8; 32]),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignRequest {
    pub operation_id: OperationId,
    pub key: KeyLocator,
    pub payload: SignablePayload,
    pub scheme: SignatureScheme,
    pub encoding: SignatureEncoding,
    pub key_tweak: Option<KeyTweak>,
    pub user_interaction: UserInteraction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_id_enforces_shared_wire_invariants() {
        assert!(OperationId::new("operation-1").is_ok());
        for invalid in ["", "contains whitespace", "contains\ncontrol"] {
            let error = OperationId::new(invalid).expect_err("invalid operation ID must fail");
            assert_eq!(error.kind, SignerErrorKind::InvalidRequest);
        }
        let error = OperationId::new("x".repeat(257))
            .expect_err("operation ID above the byte limit must fail");
        assert_eq!(error.kind, SignerErrorKind::InvalidRequest);
    }

    #[test]
    fn child_operation_id_reuses_validation() {
        let root = OperationId::new("transaction-7").expect("root operation ID must be valid");
        assert_eq!(
            root.child("input-2")
                .expect("child operation ID must be valid")
                .as_str(),
            "transaction-7:input-2"
        );
        assert!(root.child("invalid suffix").is_err());
    }
}
