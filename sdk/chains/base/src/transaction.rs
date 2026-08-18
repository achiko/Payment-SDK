use std::{error::Error as StdError, fmt, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

pub type TransactionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidAddress,
    InvalidAmount,
    InvalidSnapshot,
    InvalidTransaction,
    Unsupported,
    InsufficientFunds,
    Fee,
    Signing,
    Unavailable,
    Divergent,
    Rejected,
    Unknown,
    Timeout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

/// Canonical, chain-native textual transaction identifier.
///
/// The concrete chain validates and constructs this value. Keeping its native
/// text form makes it identical to the identifier emitted by indexing while
/// preserving each protocol's canonical formatting rules.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope(Vec<u8>);

impl Envelope {
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Envelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Envelope")
            .field("bytes", &"[REDACTED]")
            .field("length", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    version: u16,
    kind: String,
    value: serde_json::Value,
}

impl Snapshot {
    pub const VERSION: u16 = 1;

    #[must_use]
    pub fn new(kind: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            version: Self::VERSION,
            kind: kind.into(),
            value,
        }
    }

    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub const fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

/// Chain-independent transaction construction used by wallet consumers.
pub trait TransactionBuilder: Send {
    fn transfer(
        &mut self,
        destination: crate::Address,
        amount: crate::Decimal,
    ) -> Result<(), Error>;

    fn snapshot(&self) -> Result<Snapshot, Error>;

    fn prepare<'a>(&'a mut self) -> TransactionFuture<'a, Result<SignedTransaction, Error>>;
}

/// Exact signed transaction ready for submission.
///
/// This is durable data, not a live RPC handle. Persisting it before the
/// external effect allows a retry to broadcast the exact same signed bytes
/// without rebuilding or signing again.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTransaction {
    version: u16,
    kind: String,
    id: Id,
    envelope: Envelope,
}

impl SignedTransaction {
    pub const VERSION: u16 = 1;

    #[must_use]
    pub fn new(kind: impl Into<String>, id: Id, envelope: Envelope) -> Self {
        Self {
            version: Self::VERSION,
            kind: kind.into(),
            id,
            envelope,
        }
    }

    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub const fn id(&self) -> &Id {
        &self.id
    }

    #[must_use]
    pub const fn envelope(&self) -> &Envelope {
        &self.envelope
    }
}

impl fmt::Debug for SignedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedTransaction")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("id", &self.id)
            .field("envelope", &self.envelope)
            .finish()
    }
}

/// The only external effect in transaction submission.
pub trait Broadcaster: Send + Sync {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
    ) -> TransactionFuture<'a, Result<Submission, Error>>;
}

/// A transaction accepted for submission by at least one node.
///
/// Confirmation is observed through indexing and history; wallet code never
/// polls a node waiting for finality.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub id: Id,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_as_versioned_json() {
        let snapshot = Snapshot::new("fixture", serde_json::json!({ "nonce": 7 }));
        let json = serde_json::to_string(&snapshot).expect("snapshot must serialize");
        let decoded: Snapshot = serde_json::from_str(&json).expect("snapshot must deserialize");

        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.version(), Snapshot::VERSION);
        assert_eq!(decoded.kind(), "fixture");
    }

    #[test]
    fn envelope_debug_redacts_signed_bytes() {
        let debug = format!("{:?}", Envelope::new([1, 2, 3]));

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("1, 2, 3"));
    }

    #[test]
    fn prepared_round_trips_without_exposing_its_envelope_in_debug() {
        let prepared = SignedTransaction::new(
            "fixture.signed.v1",
            Id::new("canonical-id"),
            Envelope::new([1, 2, 3]),
        );
        let json = serde_json::to_vec(&prepared).expect("prepared transaction must serialize");
        let decoded: SignedTransaction =
            serde_json::from_slice(&json).expect("prepared transaction must deserialize");

        assert_eq!(decoded, prepared);
        assert!(!format!("{prepared:?}").contains("1, 2, 3"));
    }
}
