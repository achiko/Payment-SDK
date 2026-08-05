use std::{error::Error, fmt, str::FromStr};

use chain_identity::{CanonicalAddress, ChainId};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EthereumAddress(pub [u8; 20]);

impl fmt::Display for EthereumAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for EthereumAddress {
    type Err = EthereumAddressParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let hexadecimal = input
            .strip_prefix("0x")
            .ok_or(EthereumAddressParseError::MissingPrefix)?;
        if hexadecimal.len() != 40 {
            return Err(EthereumAddressParseError::InvalidLength);
        }

        let mut bytes = [0_u8; 20];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hexadecimal[index * 2..index * 2 + 2], 16)
                .map_err(|_| EthereumAddressParseError::InvalidHexadecimal)?;
        }
        Ok(Self(bytes))
    }
}

impl From<EthereumAddress> for CanonicalAddress {
    fn from(address: EthereumAddress) -> Self {
        Self {
            chain: ChainId("ethereum".to_owned()),
            value: address.to_string(),
        }
    }
}

impl TryFrom<&CanonicalAddress> for EthereumAddress {
    type Error = EthereumAddressParseError;

    fn try_from(address: &CanonicalAddress) -> Result<Self, Self::Error> {
        if address.chain != ChainId("ethereum".to_owned()) {
            return Err(EthereumAddressParseError::WrongChain);
        }
        address.value.parse()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EthereumAddressParseError {
    MissingPrefix,
    InvalidLength,
    InvalidHexadecimal,
    WrongChain,
}

impl fmt::Display for EthereumAddressParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingPrefix => "Ethereum address is missing its 0x prefix",
            Self::InvalidLength => "Ethereum address must contain exactly 20 bytes",
            Self::InvalidHexadecimal => "Ethereum address contains invalid hexadecimal",
            Self::WrongChain => "canonical address does not belong to Ethereum",
        })
    }
}

impl Error for EthereumAddressParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_canonicalizes_mixed_case_address() {
        let address = "0xAAbbccDDeeFF0011223344556677889900AaBbCc"
            .parse::<EthereumAddress>()
            .expect("valid Ethereum address must parse");
        assert_eq!(
            address.to_string(),
            "0xaabbccddeeff0011223344556677889900aabbcc"
        );
    }

    #[test]
    fn rejects_wrong_lengths_prefixes_and_characters() {
        assert!("aabb".parse::<EthereumAddress>().is_err());
        assert!("0xaabb".parse::<EthereumAddress>().is_err());
        assert!(
            "0xggbbccddeeff0011223344556677889900aabbcc"
                .parse::<EthereumAddress>()
                .is_err()
        );
    }

    #[test]
    fn canonical_conversion_rejects_foreign_chain() {
        let address = CanonicalAddress {
            chain: ChainId("bitcoin".to_owned()),
            value: "0xaabbccddeeff0011223344556677889900aabbcc".to_owned(),
        };
        assert_eq!(
            EthereumAddress::try_from(&address),
            Err(EthereumAddressParseError::WrongChain)
        );
    }
}
