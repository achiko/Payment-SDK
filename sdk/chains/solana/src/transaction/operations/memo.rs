use std::fmt;

use crate::{Error, ErrorKind};

/// Opaque 256-bit token carried by one Memo-v3 operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Memo([u8; Self::LENGTH]);

impl Memo {
    pub const LENGTH: usize = 32;

    #[must_use]
    pub fn from_bytes(bytes: [u8; Self::LENGTH]) -> Self {
        Self(bytes)
    }

    pub fn generate() -> Result<Self, Error> {
        Self::generate_with(|bytes| getrandom::fill(bytes).map_err(|_| ()))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::LENGTH] {
        &self.0
    }

    fn generate_with(
        mut fill: impl FnMut(&mut [u8; Self::LENGTH]) -> Result<(), ()>,
    ) -> Result<Self, Error> {
        let mut bytes = [0_u8; Self::LENGTH];
        fill(&mut bytes).map_err(|()| {
            Error::new(
                ErrorKind::Generation,
                "operating system random source is unavailable",
            )
        })?;
        Ok(Self(bytes))
    }
}

impl fmt::Display for Memo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&bs58::encode(self.0).into_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_opaque_token_bytes() {
        let bytes = [19; Memo::LENGTH];
        assert_eq!(Memo::from_bytes(bytes).as_bytes(), &bytes);
    }

    #[test]
    fn generates_canonical_distinct_tokens_and_reports_rng_failure() {
        let first = Memo::generate_with(|bytes| {
            *bytes = [1; Memo::LENGTH];
            Ok(())
        })
        .expect("first token");
        let second = Memo::generate_with(|bytes| {
            *bytes = [2; Memo::LENGTH];
            Ok(())
        })
        .expect("second token");

        assert_ne!(first, second);
        assert_eq!(
            bs58::decode(first.to_string())
                .into_vec()
                .expect("canonical Base58 token"),
            first.as_bytes()
        );
        assert_eq!(
            Memo::generate_with(|_| Err(()))
                .expect_err("injected failure")
                .kind(),
            ErrorKind::Generation
        );
    }
}
