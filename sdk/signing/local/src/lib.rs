//! Ephemeral local key provisioning and signing for tests and development experiments.
//!
//! Keys exist only in this process and disappear when [`LocalSigner`] is dropped.
//! This crate deliberately provides no private-key export or production custody policy.

use alloy_primitives::B256;
use alloy_signer::SignerSync as AlloySignerSync;
use alloy_signer_local::PrivateKeySigner;
use k256::{
    NonZeroScalar, Scalar, ecdsa::signature::hazmat::PrehashSigner, elliptic_curve::PrimeField,
    schnorr::SigningKey as SchnorrSigningKey,
};
use signer::{
    BoxFuture, Curve, KeyLocator, KeyProvisionRequest, KeyProvisioner, KeyTweak, KeyTweakKind,
    ProvisionedKey, PublicKey, PublicKeyFormat, SignRequest, SignablePayload, Signature,
    SignatureEncoding, SignatureScheme, Signer, SignerCapabilities, SignerError, SignerErrorKind,
    SignerStatus,
};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    fmt,
    sync::Mutex,
};

/// In-memory secp256k1 key provisioner intended only for tests and local experiments.
///
/// The generated private keys are retained so signing can resolve their opaque
/// locators. They are never returned by this API and are not persisted.
pub struct LocalSigner {
    keys: Mutex<BTreeMap<KeyLocator, PrivateKeySigner>>,
}

impl LocalSigner {
    /// Creates an empty, process-local key store.
    ///
    /// All generated keys are lost when this value is dropped or the process exits.
    #[must_use]
    pub fn ephemeral_for_testing() -> Self {
        Self {
            keys: Mutex::new(BTreeMap::new()),
        }
    }

    fn provision_key(&self, request: KeyProvisionRequest) -> Result<ProvisionedKey, SignerError> {
        if request.curve != Curve::Secp256k1 {
            return Err(SignerError {
                kind: SignerErrorKind::UnsupportedCurve,
                message: format!("local test keys do not support {:?}", request.curve),
            });
        }

        if request.purpose.trim().is_empty() {
            return Err(SignerError {
                kind: SignerErrorKind::InvalidRequest,
                message: "key purpose must not be empty".to_owned(),
            });
        }

        let signer = PrivateKeySigner::random();
        let raw_public_key = signer.public_key();
        let public_key = PublicKey {
            curve: Curve::Secp256k1,
            format: request.public_key_format,
            bytes: encode_public_key(raw_public_key.as_slice(), request.public_key_format),
        };
        let locator = locator_for(raw_public_key.as_slice());

        let mut keys = self.keys.lock().map_err(|_| SignerError {
            kind: SignerErrorKind::Unavailable,
            message: "local test key store is unavailable".to_owned(),
        })?;

        match keys.entry(locator.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(signer);
            }
            Entry::Occupied(_) => {
                return Err(SignerError {
                    kind: SignerErrorKind::Other,
                    message: "generated a duplicate local test key locator".to_owned(),
                });
            }
        }

        Ok(ProvisionedKey {
            locator,
            public_key,
        })
    }
}

impl fmt::Debug for LocalSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_count = self.keys.lock().map_or(0, |keys| keys.len());
        formatter
            .debug_struct("LocalSigner")
            .field("ephemeral_key_count", &key_count)
            .finish()
    }
}

impl KeyProvisioner for LocalSigner {
    fn provision<'a>(
        &'a self,
        request: KeyProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionedKey, SignerError>> {
        Box::pin(async move { self.provision_key(request) })
    }
}

impl Signer for LocalSigner {
    fn capabilities(&self) -> SignerCapabilities {
        SignerCapabilities {
            curves: vec![Curve::Secp256k1],
            schemes: vec![
                SignatureScheme::EcdsaSecp256k1,
                SignatureScheme::SchnorrSecp256k1,
            ],
            key_tweaks: vec![KeyTweakKind::Secp256k1Add],
            can_sign_messages: false,
            can_sign_digests: true,
            requires_user_interaction: false,
        }
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<SignerStatus, SignerError>> {
        Box::pin(async { Ok(SignerStatus::Available) })
    }

    fn public_key<'a>(
        &'a self,
        key: &'a KeyLocator,
        curve: Curve,
        format: PublicKeyFormat,
    ) -> BoxFuture<'a, Result<PublicKey, SignerError>> {
        Box::pin(async move {
            if curve != Curve::Secp256k1 {
                return Err(signer_error(
                    SignerErrorKind::UnsupportedCurve,
                    format!("local test keys do not support {curve:?}"),
                ));
            }

            let keys = self.keys.lock().map_err(|_| {
                signer_error(
                    SignerErrorKind::Unavailable,
                    "local test key store is unavailable",
                )
            })?;
            let local_key = keys.get(key).ok_or_else(|| {
                signer_error(SignerErrorKind::KeyNotFound, "local test key was not found")
            })?;

            Ok(PublicKey {
                curve,
                format,
                bytes: encode_public_key(local_key.public_key().as_slice(), format),
            })
        })
    }

    fn sign<'a>(&'a self, request: SignRequest) -> BoxFuture<'a, Result<Signature, SignerError>> {
        Box::pin(async move { self.sign_digest(request) })
    }
}

impl LocalSigner {
    fn sign_digest(&self, request: SignRequest) -> Result<Signature, SignerError> {
        let SignablePayload::Digest(digest) = request.payload else {
            return Err(signer_error(
                SignerErrorKind::UnsupportedOperation,
                "local test signer accepts precomputed digests only",
            ));
        };
        let digest: [u8; 32] = digest.bytes.try_into().map_err(|_| {
            signer_error(
                SignerErrorKind::InvalidRequest,
                "signing digest must contain exactly 32 bytes",
            )
        })?;

        let keys = self.keys.lock().map_err(|_| {
            signer_error(
                SignerErrorKind::Unavailable,
                "local test key store is unavailable",
            )
        })?;
        let local_key = keys.get(&request.key).ok_or_else(|| {
            signer_error(SignerErrorKind::KeyNotFound, "local test key was not found")
        })?;

        let bytes = match request.scheme {
            SignatureScheme::EcdsaSecp256k1 => {
                if request.key_tweak.is_some() {
                    return Err(signer_error(
                        SignerErrorKind::UnsupportedOperation,
                        "ECDSA signing does not accept a key tweak",
                    ));
                }
                sign_ecdsa(local_key, digest, request.encoding)?
            }
            SignatureScheme::SchnorrSecp256k1 => {
                sign_schnorr(local_key, digest, request.encoding, request.key_tweak)?
            }
            scheme => {
                return Err(signer_error(
                    SignerErrorKind::UnsupportedScheme,
                    format!("local test signer does not support {scheme:?}"),
                ));
            }
        };

        Ok(Signature {
            scheme: request.scheme,
            encoding: request.encoding,
            bytes,
        })
    }
}

fn sign_ecdsa(
    key: &PrivateKeySigner,
    digest: [u8; 32],
    encoding: SignatureEncoding,
) -> Result<Vec<u8>, SignerError> {
    let signature = AlloySignerSync::sign_hash_sync(key, &B256::from(digest)).map_err(|error| {
        signer_error(
            SignerErrorKind::Other,
            format!("local ECDSA signing failed: {error}"),
        )
    })?;
    let recoverable = signature.as_bytes();

    match encoding {
        SignatureEncoding::Recoverable => Ok(recoverable.to_vec()),
        SignatureEncoding::Compact | SignatureEncoding::Raw => Ok(recoverable[..64].to_vec()),
        SignatureEncoding::Der => signature
            .to_k256()
            .map(|signature| signature.to_der().as_bytes().to_vec())
            .map_err(|error| {
                signer_error(
                    SignerErrorKind::Other,
                    format!("could not encode local ECDSA signature as DER: {error}"),
                )
            }),
    }
}

fn sign_schnorr(
    key: &PrivateKeySigner,
    digest: [u8; 32],
    encoding: SignatureEncoding,
    key_tweak: Option<KeyTweak>,
) -> Result<Vec<u8>, SignerError> {
    if encoding != SignatureEncoding::Raw {
        return Err(signer_error(
            SignerErrorKind::UnsupportedOperation,
            "Schnorr signatures require raw encoding",
        ));
    }

    let mut signing_key = SchnorrSigningKey::from_bytes(key.credential().to_bytes().as_ref())
        .map_err(|error| {
            signer_error(
                SignerErrorKind::Other,
                format!("could not initialize local Schnorr key: {error}"),
            )
        })?;
    if let Some(KeyTweak::Secp256k1Add(bytes)) = key_tweak {
        let tweak = Option::<Scalar>::from(Scalar::from_repr(bytes.into())).ok_or_else(|| {
            signer_error(
                SignerErrorKind::InvalidRequest,
                "secp256k1 key tweak is not a valid scalar",
            )
        })?;
        let tweaked = *signing_key.as_nonzero_scalar().as_ref() + tweak;
        let tweaked =
            Option::<NonZeroScalar>::from(NonZeroScalar::new(tweaked)).ok_or_else(|| {
                signer_error(
                    SignerErrorKind::InvalidRequest,
                    "secp256k1 key tweak produced the zero key",
                )
            })?;
        signing_key = SchnorrSigningKey::from(tweaked);
    }

    let signature = PrehashSigner::sign_prehash(&signing_key, &digest).map_err(|error| {
        signer_error(
            SignerErrorKind::Other,
            format!("local Schnorr signing failed: {error}"),
        )
    })?;
    Ok(signature.to_bytes().to_vec())
}

fn signer_error(kind: SignerErrorKind, message: impl Into<String>) -> SignerError {
    SignerError {
        kind,
        message: message.into(),
    }
}

fn encode_public_key(raw: &[u8], format: PublicKeyFormat) -> Vec<u8> {
    debug_assert_eq!(raw.len(), 64);

    match format {
        PublicKeyFormat::Raw => raw.to_vec(),
        PublicKeyFormat::XOnly => raw[..32].to_vec(),
        PublicKeyFormat::Uncompressed => {
            let mut encoded = Vec::with_capacity(65);
            encoded.push(0x04);
            encoded.extend_from_slice(raw);
            encoded
        }
        PublicKeyFormat::Compressed => {
            let mut encoded = Vec::with_capacity(33);
            encoded.push(if raw[63] & 1 == 0 { 0x02 } else { 0x03 });
            encoded.extend_from_slice(&raw[..32]);
            encoded
        }
    }
}

fn locator_for(raw_public_key: &[u8]) -> KeyLocator {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut identifier = String::with_capacity("ephemeral:secp256k1:".len() + 128);
    identifier.push_str("ephemeral:secp256k1:");
    for byte in raw_public_key {
        identifier.push(char::from(HEX[usize::from(byte >> 4)]));
        identifier.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }

    KeyLocator::Identifier(identifier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);

    fn request(curve: Curve, format: PublicKeyFormat) -> KeyProvisionRequest {
        KeyProvisionRequest {
            operation_id: signer::OperationId::new(format!(
                "local-provision-{}",
                NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed)
            ))
            .expect("test operation ID must be valid"),
            curve,
            public_key_format: format,
            purpose: "test-deposit".to_owned(),
        }
    }

    #[test]
    fn provisions_distinct_ephemeral_keys() {
        let keys = LocalSigner::ephemeral_for_testing();

        let first = block_on(keys.provision(request(Curve::Secp256k1, PublicKeyFormat::Raw)))
            .expect("first key should be generated");
        let second = block_on(keys.provision(request(Curve::Secp256k1, PublicKeyFormat::Raw)))
            .expect("second key should be generated");

        assert_ne!(first.locator, second.locator);
        assert_ne!(first.public_key.bytes, second.public_key.bytes);
        assert_eq!(first.public_key.bytes.len(), 64);
        assert_eq!(
            format!("{keys:?}"),
            "LocalSigner { ephemeral_key_count: 2 }"
        );
    }

    #[test]
    fn returns_each_requested_public_key_encoding() {
        let keys = LocalSigner::ephemeral_for_testing();

        let compressed =
            block_on(keys.provision(request(Curve::Secp256k1, PublicKeyFormat::Compressed)))
                .expect("compressed key should be generated");
        let uncompressed =
            block_on(keys.provision(request(Curve::Secp256k1, PublicKeyFormat::Uncompressed)))
                .expect("uncompressed key should be generated");
        let x_only = block_on(keys.provision(request(Curve::Secp256k1, PublicKeyFormat::XOnly)))
            .expect("x-only key should be generated");

        assert_eq!(compressed.public_key.bytes.len(), 33);
        assert!(matches!(compressed.public_key.bytes[0], 0x02 | 0x03));
        assert_eq!(uncompressed.public_key.bytes.len(), 65);
        assert_eq!(uncompressed.public_key.bytes[0], 0x04);
        assert_eq!(x_only.public_key.bytes.len(), 32);
    }

    #[test]
    fn rejects_unsupported_curves_and_empty_purposes() {
        let keys = LocalSigner::ephemeral_for_testing();

        for curve in [Curve::Ed25519, Curve::NistP256] {
            let error = block_on(keys.provision(request(curve, PublicKeyFormat::Raw)))
                .expect_err("unsupported curve should fail");
            assert_eq!(error.kind, SignerErrorKind::UnsupportedCurve);
        }

        let error = block_on(
            keys.provision(KeyProvisionRequest {
                operation_id: signer::OperationId::new("local-empty-purpose")
                    .expect("test operation ID must be valid"),
                curve: Curve::Secp256k1,
                public_key_format: PublicKeyFormat::Raw,
                purpose: "  ".to_owned(),
            }),
        )
        .expect_err("empty purpose should fail");
        assert_eq!(error.kind, SignerErrorKind::InvalidRequest);
    }

    #[test]
    fn signs_ecdsa_and_schnorr_digests_by_locator() {
        let signer = LocalSigner::ephemeral_for_testing();
        let key =
            block_on(signer.provision(request(Curve::Secp256k1, PublicKeyFormat::Compressed)))
                .expect("test key should be provisioned");
        let digest = signer::Digest { bytes: vec![7; 32] };

        let recoverable = block_on(
            signer.sign(SignRequest {
                operation_id: signer::OperationId::new("local-sign-recoverable")
                    .expect("test operation ID must be valid"),
                key: key.locator.clone(),
                payload: SignablePayload::Digest(digest.clone()),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Recoverable,
                key_tweak: None,
                user_interaction: signer::UserInteraction::NotRequired,
            }),
        )
        .expect("ECDSA digest should be signed");
        let der = block_on(
            signer.sign(SignRequest {
                operation_id: signer::OperationId::new("local-sign-der")
                    .expect("test operation ID must be valid"),
                key: key.locator.clone(),
                payload: SignablePayload::Digest(digest.clone()),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Der,
                key_tweak: None,
                user_interaction: signer::UserInteraction::NotRequired,
            }),
        )
        .expect("ECDSA digest should be DER encoded");
        let schnorr = block_on(
            signer.sign(SignRequest {
                operation_id: signer::OperationId::new("local-sign-schnorr")
                    .expect("test operation ID must be valid"),
                key: key.locator,
                payload: SignablePayload::Digest(digest),
                scheme: SignatureScheme::SchnorrSecp256k1,
                encoding: SignatureEncoding::Raw,
                key_tweak: Some(KeyTweak::Secp256k1Add([3; 32])),
                user_interaction: signer::UserInteraction::NotRequired,
            }),
        )
        .expect("tweaked Schnorr digest should be signed");

        assert_eq!(recoverable.bytes.len(), 65);
        assert!(k256::ecdsa::Signature::from_der(&der.bytes).is_ok());
        assert_eq!(schnorr.bytes.len(), 64);
    }
}
