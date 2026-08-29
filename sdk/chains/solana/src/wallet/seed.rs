use std::str::FromStr;

use wallets::SecretBytes;
use zeroize::Zeroizing;

use crate::{Error, ErrorKind};

/// One imported 32-byte Ed25519 seed held by the shared zeroizing container.
///
/// This value intentionally implements neither `Clone`, `Debug`, `Display`,
/// nor Serde, so accepted secret material has no ordinary output path.
pub struct Seed(SecretBytes);

impl Seed {
    pub(super) fn into_secret(self) -> SecretBytes {
        self.0
    }
}

impl FromStr for Seed {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.len() != 64
            || !input
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid_seed());
        }
        let mut bytes = Zeroizing::new([0_u8; 32]);
        hex::decode_to_slice(input, bytes.as_mut()).map_err(|_| invalid_seed())?;
        Ok(Self(SecretBytes::new(*bytes)))
    }
}

fn invalid_seed() -> Error {
    Error::new(
        ErrorKind::InvalidSecret,
        "Solana secret must be exactly 64 lowercase hexadecimal characters",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reject(input: &str) {
        let error = match input.parse::<Seed>() {
            Ok(_) => panic!("invalid seed unexpectedly parsed"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::InvalidSecret);
        if !input.is_empty() {
            assert!(!error.to_string().contains(input));
            assert!(!format!("{error:?}").contains(input));
        }
    }

    #[test]
    fn accepts_exact_lowercase_hex_seed() {
        let seed = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
            .parse::<Seed>()
            .expect("canonical seed");
        assert_eq!(seed.0.as_bytes(), &(0_u8..32).collect::<Vec<_>>());
    }

    #[test]
    fn rejects_every_alternate_boundary_encoding() {
        for input in [
            "",
            "00",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f00",
            "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
            " 00102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f ",
            "gg0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        ] {
            reject(input);
        }
    }
}
