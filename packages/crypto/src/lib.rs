//! Reusable cryptographic values and secp256k1 operations.
//!
//! This crate contains no chain, wallet, transaction, RPC, or custody policy.

mod error;
mod key;
mod secret;
mod signature;

pub use error::{Error, ErrorKind};
pub use key::{Curve, PublicKey, PublicKeyFormat, ScalarTweak, SecretKey};
pub use secret::SecretBytes;
pub use signature::{Signature, SignatureEncoding, SignatureScheme};
