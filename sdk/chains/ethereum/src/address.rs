use std::{error::Error, fmt, str::FromStr};

use base::{Address as BaseAddress, AddressError, AddressValidator, Addresser};
use indexing::{CanonicalAddress, ChainId};

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Address(pub [u8; 20]);

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Addresser for Address {
    fn address(&self) -> BaseAddress {
        BaseAddress::from(self.0)
    }
}

impl AddressValidator for Address {
    fn validate(&self) -> Result<(), AddressError> {
        Ok(())
    }
}

impl FromStr for Address {
    type Err = AddressParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let hexadecimal = input
            .strip_prefix("0x")
            .ok_or(AddressParseError::MissingPrefix)?;
        if hexadecimal.len() != 40 {
            return Err(AddressParseError::InvalidLength);
        }

        let mut bytes = [0_u8; 20];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hexadecimal[index * 2..index * 2 + 2], 16)
                .map_err(|_| AddressParseError::InvalidHexadecimal)?;
        }
        Ok(Self(bytes))
    }
}

impl Address {
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }

    #[must_use]
    pub fn canonical(&self, scope: indexing::IndexScope) -> CanonicalAddress {
        CanonicalAddress {
            scope,
            value: self.to_string(),
        }
    }
}

impl TryFrom<&CanonicalAddress> for Address {
    type Error = AddressParseError;

    fn try_from(address: &CanonicalAddress) -> Result<Self, Self::Error> {
        if address.scope.chain != ChainId(crate::CHAIN.to_owned()) {
            return Err(AddressParseError::WrongChain);
        }
        address.value.parse()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressParseError {
    MissingPrefix,
    InvalidLength,
    InvalidHexadecimal,
    WrongChain,
}

impl fmt::Display for AddressParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingPrefix => "Ethereum address is missing its 0x prefix",
            Self::InvalidLength => "Ethereum address must contain exactly 20 bytes",
            Self::InvalidHexadecimal => "Ethereum address contains invalid hexadecimal",
            Self::WrongChain => "canonical address does not belong to Ethereum",
        })
    }
}

impl Error for AddressParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_canonicalizes_mixed_case_address() {
        let address = "0xAAbbccDDeeFF0011223344556677889900AaBbCc"
            .parse::<Address>()
            .expect("valid Ethereum address must parse");
        assert_eq!(
            address.to_string(),
            "0xaabbccddeeff0011223344556677889900aabbcc"
        );
    }

    #[test]
    fn rejects_wrong_lengths_prefixes_and_characters() {
        assert!("aabb".parse::<Address>().is_err());
        assert!("0xaabb".parse::<Address>().is_err());
        assert!(
            "0xggbbccddeeff0011223344556677889900aabbcc"
                .parse::<Address>()
                .is_err()
        );
    }

    #[test]
    fn canonical_conversion_rejects_foreign_chain() {
        let address = CanonicalAddress {
            scope: indexing::IndexScope {
                chain: ChainId("bitcoin".to_owned()),
                network: "mainnet".to_owned(),
            },
            value: "0xaabbccddeeff0011223344556677889900aabbcc".to_owned(),
        };
        assert_eq!(
            Address::try_from(&address),
            Err(AddressParseError::WrongChain)
        );
    }

    #[test]
    fn detects_only_the_zero_address() {
        assert!(Address([0; 20]).is_zero());
        assert!(!Address([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]).is_zero());
    }
}
