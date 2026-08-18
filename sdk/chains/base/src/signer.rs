use std::{future::Future, pin::Pin};

use crate::{
    KeyTweak, PublicKey, PublicKeyFormat, Signature, SignatureEncoding, SignatureScheme,
    SignerError,
};

pub type SignFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SignedPayload, SignerError>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Digest {
    /// The chain or protocol computes this digest. The signer does not hash it again.
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SignablePayload {
    Message(Vec<u8>),
    Digest(Digest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignRequest {
    pub payload: SignablePayload,
    pub scheme: SignatureScheme,
    pub encoding: SignatureEncoding,
    pub public_key_format: PublicKeyFormat,
    pub key_tweak: Option<KeyTweak>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedPayload {
    pub signature: Signature,
    pub public_key: PublicKey,
}

/// Minimal cryptographic boundary shared by chain transaction implementations.
pub trait Signer: Send + Sync {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a>;
}
