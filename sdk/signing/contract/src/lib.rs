//! Chain-independent key and signing contracts.

mod error;
mod key;
mod request;
mod signature;
mod signer;

pub use error::{SignerError, SignerErrorKind};
pub use key::{
    ChildIndex, Curve, DerivationPath, KeyLocator, KeyProvisionRequest, ProvisionedKey, PublicKey,
    PublicKeyFormat,
};
pub use request::OperationId;
pub use request::{Digest, KeyTweak, SignRequest, SignablePayload, UserInteraction};
pub use signature::{Signature, SignatureEncoding, SignatureScheme};
pub use signer::{KeyProvisioner, Signer, SignerCapabilities, SignerStatus};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
