use crypto::{Error as CryptoError, SecretKey};

use crate::{
    Address, Addresser, SignRequest, SignablePayload, Signature, SignatureScheme, SignedPayload,
    Signer, SignerError, SignerErrorKind,
};

/// An address paired with an in-process secp256k1 key.
///
/// Secret storage and cryptographic operations are delegated to `packages/crypto`.
pub struct KeyPair<A> {
    pub address: A,
    key: SecretKey,
}

impl<A> KeyPair<A> {
    pub fn new(address: A, key: impl Into<Vec<u8>>) -> Result<Self, SignerError> {
        let key = SecretKey::new(key).map_err(signer_error)?;
        Ok(Self { address, key })
    }

    fn sign_now(&self, request: SignRequest) -> Result<SignedPayload, SignerError> {
        let SignablePayload::Digest(digest) = request.payload else {
            return Err(error(
                SignerErrorKind::UnsupportedOperation,
                "signer accepts precomputed digests only",
            ));
        };
        let public_key = self
            .key
            .public_key(request.public_key_format)
            .map_err(signer_error)?;
        let bytes = match request.scheme {
            SignatureScheme::EcdsaSecp256k1 => {
                if request.key_tweak.is_some() {
                    return Err(error(
                        SignerErrorKind::UnsupportedOperation,
                        "ECDSA does not accept a key tweak",
                    ));
                }
                self.key
                    .sign_ecdsa(&digest.bytes, request.encoding)
                    .map_err(signer_error)?
            }
            SignatureScheme::SchnorrSecp256k1 => self
                .key
                .sign_schnorr(&digest.bytes, request.encoding, request.key_tweak.as_ref())
                .map_err(signer_error)?,
            _ => {
                return Err(error(
                    SignerErrorKind::UnsupportedScheme,
                    "key pair supports secp256k1 only",
                ));
            }
        };
        Ok(SignedPayload {
            signature: Signature {
                scheme: request.scheme,
                encoding: request.encoding,
                bytes,
            },
            public_key,
        })
    }
}

impl<A: Addresser> Addresser for KeyPair<A> {
    fn address(&self) -> Address {
        self.address.address()
    }
}

impl<A: Send + Sync> Signer for KeyPair<A> {
    fn sign<'a>(&'a self, request: SignRequest) -> crate::SignFuture<'a> {
        Box::pin(async move { self.sign_now(request) })
    }
}

fn signer_error(error: CryptoError) -> SignerError {
    SignerError {
        kind: SignerErrorKind::InvalidRequest,
        message: error.to_string(),
    }
}

fn error(kind: SignerErrorKind, message: impl Into<String>) -> SignerError {
    SignerError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use futures_executor::block_on;

    use super::*;
    use crate::{Digest, PublicKeyFormat, SignablePayload, SignatureEncoding, SignatureScheme};

    #[test]
    fn key_pair_is_the_local_signer() {
        let pair = KeyPair::new([7_u8; 20], [1_u8; 32]).expect("test key must be valid");
        let signed = block_on(pair.sign(SignRequest {
            payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }))
        .expect("valid digest must sign");

        assert_eq!(signed.signature.bytes.len(), 65);
        assert_eq!(signed.public_key.bytes.len(), 64);
    }
}
