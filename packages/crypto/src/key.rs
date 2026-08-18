use k256::{
    NonZeroScalar, Scalar, ecdsa::SigningKey as EcdsaSigningKey,
    ecdsa::signature::hazmat::PrehashSigner, elliptic_curve::PrimeField,
    schnorr::SigningKey as SchnorrSigningKey,
};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroize;

use crate::{Error, ErrorKind, SignatureEncoding};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Curve {
    Secp256k1,
    Ed25519,
    NistP256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PublicKeyFormat {
    Compressed,
    Uncompressed,
    XOnly,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicKey {
    pub curve: Curve,
    pub format: PublicKeyFormat,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ScalarTweak {
    Add([u8; 32]),
    TaggedHashAdd { tag: Vec<u8>, suffix: Vec<u8> },
}

/// An in-memory secp256k1 secret scalar.
///
/// This value intentionally implements neither `Clone`, `Debug`, nor Serde.
pub struct SecretKey {
    bytes: Vec<u8>,
}

impl SecretKey {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, Error> {
        let bytes = bytes.into();
        EcdsaSigningKey::from_slice(&bytes).map_err(|_| {
            Error::new(
                ErrorKind::InvalidKey,
                "secret key must be a valid 32-byte secp256k1 scalar",
            )
        })?;
        Ok(Self { bytes })
    }

    pub fn public_key(&self, format: PublicKeyFormat) -> Result<PublicKey, Error> {
        let key = self.ecdsa_key()?;
        let encoded = key.verifying_key().to_encoded_point(false);
        let raw = &encoded.as_bytes()[1..];
        let bytes = match format {
            PublicKeyFormat::Raw => raw.to_vec(),
            PublicKeyFormat::XOnly => raw[..32].to_vec(),
            PublicKeyFormat::Uncompressed => encoded.as_bytes().to_vec(),
            PublicKeyFormat::Compressed => key
                .verifying_key()
                .to_encoded_point(true)
                .as_bytes()
                .to_vec(),
        };
        Ok(PublicKey {
            curve: Curve::Secp256k1,
            format,
            bytes,
        })
    }

    pub fn sign_ecdsa(&self, digest: &[u8], encoding: SignatureEncoding) -> Result<Vec<u8>, Error> {
        let digest = digest_array(digest)?;
        let (signature, recovery_id) = self
            .ecdsa_key()?
            .sign_prehash_recoverable(&digest)
            .map_err(|_| Error::new(ErrorKind::Signing, "ECDSA signing failed"))?;
        match encoding {
            SignatureEncoding::Recoverable => {
                let mut bytes = signature.to_bytes().to_vec();
                bytes.push(recovery_id.to_byte());
                Ok(bytes)
            }
            SignatureEncoding::Compact | SignatureEncoding::Raw => {
                Ok(signature.to_bytes().to_vec())
            }
            SignatureEncoding::Der => Ok(signature.to_der().as_bytes().to_vec()),
        }
    }

    pub fn sign_schnorr(
        &self,
        digest: &[u8],
        encoding: SignatureEncoding,
        tweak: Option<&ScalarTweak>,
    ) -> Result<Vec<u8>, Error> {
        if encoding != SignatureEncoding::Raw {
            return Err(Error::new(
                ErrorKind::UnsupportedEncoding,
                "Schnorr signatures require raw encoding",
            ));
        }
        let digest = digest_array(digest)?;
        let mut key = SchnorrSigningKey::from_bytes(&self.bytes)
            .map_err(|_| Error::new(ErrorKind::InvalidKey, "invalid Schnorr key"))?;
        if let Some(tweak) = tweak {
            let bytes = tweak.scalar(&key)?;
            let tweak = Option::<Scalar>::from(Scalar::from_repr(bytes.into()))
                .ok_or_else(|| Error::new(ErrorKind::InvalidTweak, "invalid scalar tweak"))?;
            let scalar = *key.as_nonzero_scalar().as_ref() + tweak;
            let scalar =
                Option::<NonZeroScalar>::from(NonZeroScalar::new(scalar)).ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidTweak,
                        "scalar tweak produced the zero key",
                    )
                })?;
            key = SchnorrSigningKey::from(scalar);
        }
        let signature: k256::schnorr::Signature = PrehashSigner::sign_prehash(&key, &digest)
            .map_err(|_| Error::new(ErrorKind::Signing, "Schnorr signing failed"))?;
        Ok(signature.to_bytes().to_vec())
    }

    fn ecdsa_key(&self) -> Result<EcdsaSigningKey, Error> {
        EcdsaSigningKey::from_slice(&self.bytes)
            .map_err(|_| Error::new(ErrorKind::InvalidKey, "invalid secp256k1 secret key"))
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl ScalarTweak {
    fn scalar(&self, key: &SchnorrSigningKey) -> Result<[u8; 32], Error> {
        match self {
            Self::Add(bytes) => Ok(*bytes),
            Self::TaggedHashAdd { tag, suffix } => {
                if tag.is_empty() {
                    return Err(Error::new(ErrorKind::InvalidTweak, "tag must not be empty"));
                }
                let tag_hash = Sha256::digest(tag);
                let mut hash = Sha256::new();
                hash.update(tag_hash);
                hash.update(tag_hash);
                hash.update(key.verifying_key().to_bytes());
                hash.update(suffix);
                Ok(hash.finalize().into())
            }
        }
    }
}

fn digest_array(digest: &[u8]) -> Result<[u8; 32], Error> {
    digest.try_into().map_err(|_| {
        Error::new(
            ErrorKind::InvalidDigest,
            "digest must contain exactly 32 bytes",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_ecdsa_and_schnorr_digests() {
        let key = SecretKey::new([1_u8; 32]).expect("test key must be valid");
        assert_eq!(
            key.sign_ecdsa(&[2; 32], SignatureEncoding::Recoverable)
                .expect("ECDSA must sign")
                .len(),
            65
        );
        assert_eq!(
            key.sign_schnorr(&[3; 32], SignatureEncoding::Raw, None)
                .expect("Schnorr must sign")
                .len(),
            64
        );
    }
}
