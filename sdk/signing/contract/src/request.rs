use crate::{KeyLocator, SignatureEncoding, SignatureScheme};

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
    pub key: KeyLocator,
    pub payload: SignablePayload,
    pub scheme: SignatureScheme,
    pub encoding: SignatureEncoding,
    pub key_tweak: Option<KeyTweak>,
    pub user_interaction: UserInteraction,
}
