use base::{
    Curve, PublicKey, PublicKeyFormat, SignRequest, SignablePayload, Signature, SignatureEncoding,
    SignatureScheme, SignedPayload, Signer, SignerError, SignerErrorKind, TransactionId,
};
use solana_keypair::{Keypair, Signer as NativeSigner};
use solana_signature::Signature as NativeSignature;
use wallets::SecretBytes;
use zeroize::Zeroizing;

use crate::{Address, Error, ErrorKind};

use super::Seed;

/// One private Ed25519 key owner.
///
/// The secret remains in `SecretBytes`; this type intentionally implements no
/// cloning, formatting, debugging, or serialization traits.
pub struct Key {
    secret: SecretBytes,
    address: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedMessage {
    signature: [u8; 64],
    id: TransactionId,
}

impl Key {
    pub fn from_seed(seed: Seed) -> Result<Self, Error> {
        Self::from_secret(seed.into_secret())
    }

    pub fn generate() -> Result<Self, Error> {
        Self::generate_with(|bytes| getrandom::fill(bytes).map_err(|_| ()))
    }

    #[must_use]
    pub fn address(&self) -> &Address {
        &self.address
    }

    pub fn sign_message(&self, message: &[u8]) -> Result<SignedMessage, Error> {
        let signature = self.native_key()?.sign_message(message);
        SignedMessage::verified(&self.address, message, signature)
    }

    fn from_secret(secret: SecretBytes) -> Result<Self, Error> {
        let bytes: [u8; 32] = secret.as_bytes().try_into().map_err(|_| {
            Error::new(
                ErrorKind::InvalidSecret,
                "Solana secret must contain exactly 32 bytes",
            )
        })?;
        let key = Keypair::new_from_array(bytes);
        Ok(Self {
            secret,
            address: Address::from_bytes(key.pubkey().to_bytes()),
        })
    }

    fn generate_with(mut fill: impl FnMut(&mut [u8; 32]) -> Result<(), ()>) -> Result<Self, Error> {
        let mut bytes = Zeroizing::new([0_u8; 32]);
        fill(&mut bytes).map_err(|()| {
            Error::new(
                ErrorKind::Generation,
                "operating system random source is unavailable",
            )
        })?;
        Self::from_secret(SecretBytes::new(*bytes))
    }

    fn native_key(&self) -> Result<Keypair, Error> {
        let bytes: [u8; 32] = self
            .secret
            .as_bytes()
            .try_into()
            .map_err(|_| Error::new(ErrorKind::InvalidSecret, "Solana key owner is invalid"))?;
        Ok(Keypair::new_from_array(bytes))
    }
}

impl SignedMessage {
    fn verified(
        address: &Address,
        message: &[u8],
        signature: NativeSignature,
    ) -> Result<Self, Error> {
        if !signature.verify(address.as_bytes(), message) {
            return Err(Error::new(
                ErrorKind::Signing,
                "Solana signature does not match the source and message",
            ));
        }
        Ok(Self {
            signature: *signature.as_array(),
            id: TransactionId::new(signature.to_string()),
        })
    }

    #[must_use]
    pub const fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    #[must_use]
    pub const fn id(&self) -> &TransactionId {
        &self.id
    }
}

impl Signer for Key {
    fn sign<'a>(&'a self, request: SignRequest) -> base::SignFuture<'a> {
        Box::pin(async move {
            if request.scheme != SignatureScheme::Ed25519 {
                return Err(signer_error(
                    SignerErrorKind::UnsupportedScheme,
                    "Solana requires Ed25519 signatures",
                ));
            }
            if request.encoding != SignatureEncoding::Raw
                || request.public_key_format != PublicKeyFormat::Raw
            {
                return Err(signer_error(
                    SignerErrorKind::InvalidRequest,
                    "Solana requires raw signatures and public keys",
                ));
            }
            if request.key_tweak.is_some() {
                return Err(signer_error(
                    SignerErrorKind::UnsupportedOperation,
                    "Solana signing does not support key tweaks",
                ));
            }
            let SignablePayload::Message(message) = request.payload else {
                return Err(signer_error(
                    SignerErrorKind::InvalidRequest,
                    "Solana signs exact messages, not caller-provided digests",
                ));
            };
            let signed = self
                .sign_message(&message)
                .map_err(|_| signer_error(SignerErrorKind::Other, "Solana signing failed"))?;
            Ok(SignedPayload {
                signature: Signature {
                    scheme: SignatureScheme::Ed25519,
                    encoding: SignatureEncoding::Raw,
                    bytes: signed.signature.to_vec(),
                },
                public_key: PublicKey {
                    curve: Curve::Ed25519,
                    format: PublicKeyFormat::Raw,
                    bytes: self.address.as_bytes().to_vec(),
                },
            })
        })
    }
}

fn signer_error(kind: SignerErrorKind, message: impl Into<String>) -> SignerError {
    SignerError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use futures_executor::block_on;

    use super::*;

    fn fixture() -> Key {
        Key::from_seed(
            "0707070707070707070707070707070707070707070707070707070707070707"
                .parse()
                .expect("fixture seed"),
        )
        .expect("fixture key")
    }

    #[test]
    fn derives_and_signs_exactly_like_the_maintained_library() {
        let key = fixture();
        let native = Keypair::new_from_array([7; 32]);
        let message = b"exact Solana wire message";
        let expected = native.sign_message(message);
        let signed = key.sign_message(message).expect("signature");

        assert_eq!(key.address().as_bytes(), &native.pubkey().to_bytes());
        assert_eq!(signed.signature(), expected.as_array());
        assert_eq!(signed.id().as_str(), expected.to_string());
        assert!(
            NativeSignature::from(*signed.signature()).verify(key.address().as_bytes(), message)
        );
    }

    #[test]
    fn rejects_a_signature_from_another_key_or_message() {
        let key = fixture();
        let other = Keypair::new_from_array([8; 32]);
        let signature = other.sign_message(b"message");

        assert_eq!(
            SignedMessage::verified(key.address(), b"message", signature)
                .unwrap_err()
                .kind(),
            ErrorKind::Signing
        );
        let signature = Keypair::new_from_array([7; 32]).sign_message(b"different");
        assert_eq!(
            SignedMessage::verified(key.address(), b"message", signature)
                .unwrap_err()
                .kind(),
            ErrorKind::Signing
        );
    }

    #[test]
    fn generates_distinct_keys_and_reports_failure_before_publication() {
        let first = Key::generate().expect("OS randomness");
        let second = Key::generate().expect("OS randomness");
        assert_ne!(first.address(), second.address());

        let calls = std::cell::Cell::new(0);
        let error = Key::generate_with(|_| {
            calls.set(calls.get() + 1);
            Err(())
        })
        .err()
        .expect("injected failure");
        assert_eq!(calls.get(), 1);
        assert_eq!(error.kind(), ErrorKind::Generation);
    }

    #[test]
    fn implements_the_exact_shared_signer_contract() {
        let key = fixture();
        let request = SignRequest {
            payload: SignablePayload::Message(b"message".to_vec()),
            scheme: SignatureScheme::Ed25519,
            encoding: SignatureEncoding::Raw,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        };
        let signed = block_on(key.sign(request)).expect("supported request");
        assert_eq!(signed.signature.bytes.len(), 64);
        assert_eq!(signed.public_key.bytes, key.address().as_bytes());

        for request in [
            SignRequest {
                scheme: SignatureScheme::EcdsaSecp256k1,
                payload: SignablePayload::Message(vec![]),
                encoding: SignatureEncoding::Raw,
                public_key_format: PublicKeyFormat::Raw,
                key_tweak: None,
            },
            SignRequest {
                scheme: SignatureScheme::Ed25519,
                payload: SignablePayload::Digest(base::Digest { bytes: vec![0; 32] }),
                encoding: SignatureEncoding::Raw,
                public_key_format: PublicKeyFormat::Raw,
                key_tweak: None,
            },
            SignRequest {
                scheme: SignatureScheme::Ed25519,
                payload: SignablePayload::Message(vec![]),
                encoding: SignatureEncoding::Der,
                public_key_format: PublicKeyFormat::Raw,
                key_tweak: None,
            },
        ] {
            assert!(block_on(key.sign(request)).is_err());
        }
    }
}
