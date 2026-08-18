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
}
