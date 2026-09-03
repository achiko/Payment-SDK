//! Minimal primitives shared by SDK capabilities and concrete protocols.

mod address;
mod asset;
mod block;
mod chain;
mod decimal;
mod derivation;
mod error;
mod key_pair;
mod network;
mod signer;
pub mod transaction;

pub use address::{Address, AddressError, AddressErrorKind, AddressValidator, Addresser};
pub use asset::{Asset, Asseter};
pub use block::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
pub use chain::{Chain, ChainCollection, TestnetMap};
pub use crypto::{
    Curve, PublicKey, PublicKeyFormat, ScalarTweak as KeyTweak, Signature, SignatureEncoding,
    SignatureScheme,
};
pub use decimal::{Decimal, DecimalError, DecimalErrorKind, DecimalParts, DecimalSign};
pub use derivation::{ChildIndex, DerivationPath};
pub use error::{Error as SignerError, ErrorKind as SignerErrorKind};
pub use key_pair::KeyPair;
pub use network::{NetworkId, NetworkKind};
pub use signer::{Digest, SignFuture, SignRequest, SignablePayload, SignedPayload, Signer};
pub use transaction::{
    Broadcaster, Envelope as TransactionEnvelope, Error as TransactionError,
    ErrorKind as TransactionErrorKind, Id, Id as TransactionId, SignedTransaction,
    Snapshot as TransactionSnapshot, Submission, TransactionBuilder, TransactionFuture,
};
