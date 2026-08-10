use crate::{
    BoxFuture, Curve, KeyLocator, KeyProvisionRequest, KeyTweakKind, ProvisionedKey, PublicKey,
    PublicKeyFormat, SignRequest, Signature, SignatureScheme, SignerError,
};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignerCapabilities {
    pub curves: Vec<Curve>,
    pub schemes: Vec<SignatureScheme>,
    pub key_tweaks: Vec<KeyTweakKind>,
    pub can_sign_messages: bool,
    pub can_sign_digests: bool,
    pub requires_user_interaction: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerStatus {
    Available,
    InteractionRequired,
    Unavailable { reason: String },
}

/// Object-safe so an application can select local, hardware, HSM, or remote
/// implementations at runtime and inject `&dyn Signer` into a chain.
pub trait Signer: Send + Sync {
    fn capabilities(&self) -> SignerCapabilities;

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<SignerStatus, SignerError>>;

    fn public_key<'a>(
        &'a self,
        key: &'a KeyLocator,
        curve: Curve,
        format: PublicKeyFormat,
    ) -> BoxFuture<'a, Result<PublicKey, SignerError>>;

    fn sign<'a>(&'a self, request: SignRequest) -> BoxFuture<'a, Result<Signature, SignerError>>;
}

/// Optional capability implemented by local, hardware, HSM, or remote key stores
/// that can allocate a new key handle. Provisioning is separate from signing.
pub trait KeyProvisioner: Send + Sync {
    fn provision<'a>(
        &'a self,
        request: KeyProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionedKey, SignerError>>;
}

impl<T> Signer for Arc<T>
where
    T: Signer + ?Sized,
{
    fn capabilities(&self) -> SignerCapabilities {
        (**self).capabilities()
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<SignerStatus, SignerError>> {
        (**self).status()
    }

    fn public_key<'a>(
        &'a self,
        key: &'a KeyLocator,
        curve: Curve,
        format: PublicKeyFormat,
    ) -> BoxFuture<'a, Result<PublicKey, SignerError>> {
        (**self).public_key(key, curve, format)
    }

    fn sign<'a>(&'a self, request: SignRequest) -> BoxFuture<'a, Result<Signature, SignerError>> {
        (**self).sign(request)
    }
}

impl<T> KeyProvisioner for Arc<T>
where
    T: KeyProvisioner + ?Sized,
{
    fn provision<'a>(
        &'a self,
        request: KeyProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionedKey, SignerError>> {
        (**self).provision(request)
    }
}
