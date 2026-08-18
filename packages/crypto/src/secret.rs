use zeroize::Zeroize;

/// Opaque in-memory secret material passed from composition into a key owner.
///
/// It intentionally implements neither `Clone`, `Debug`, `Display`, nor Serde.
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Generates a valid secp256k1 scalar from the operating system CSPRNG.
    ///
    /// Key generation lives in the generic cryptography package so wallet
    /// applications never manufacture or serialize private-key bytes.
    pub fn generate_secp256k1() -> Result<Self, crate::Error> {
        loop {
            let mut bytes = [0_u8; 32];
            getrandom::fill(&mut bytes).map_err(|_| {
                crate::Error::new(
                    crate::ErrorKind::KeyGeneration,
                    "operating system random source is unavailable",
                )
            })?;
            if k256::SecretKey::from_slice(&bytes).is_ok() {
                return Ok(Self::new(bytes));
            }
        }
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_bytes_only_by_explicit_borrow() {
        let secret = SecretBytes::new([1, 2, 3]);

        assert_eq!(secret.as_bytes(), &[1, 2, 3]);
        assert!(!secret.is_empty());
    }

    #[test]
    fn generates_valid_distinct_secp256k1_secrets() {
        let first = SecretBytes::generate_secp256k1().expect("OS randomness must be available");
        let second = SecretBytes::generate_secp256k1().expect("OS randomness must be available");

        assert!(k256::SecretKey::from_slice(first.as_bytes()).is_ok());
        assert_ne!(first.as_bytes(), second.as_bytes());
    }
}
