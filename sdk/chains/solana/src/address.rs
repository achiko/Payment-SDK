use std::{error::Error, fmt, str::FromStr};

use base::{Address as BaseAddress, Addresser};
use solana_address::{Address as NativeAddress, error::ParseAddressError as NativeParseError};
use wallets::{AddressEncoding, AddressText};

/// Exact 32-byte Solana account identity.
///
/// Parsing and rendering accept only canonical plain Base58.
///
/// # Examples
///
/// ```
/// use chain_solana::Address;
///
/// let address = "11111111111111111111111111111111"
///     .parse::<Address>()
///     .expect("the canonical zero address must parse");
/// assert_eq!(address.as_bytes(), &[0; 32]);
/// assert_eq!(address.to_string(), "11111111111111111111111111111111");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(NativeAddress);

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Addresser for Address {
    fn address(&self) -> BaseAddress {
        BaseAddress::from(*self.as_bytes())
    }
}

impl FromStr for Address {
    type Err = AddressParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let native = input
            .parse::<NativeAddress>()
            .map_err(|error| match error {
                NativeParseError::WrongSize => AddressParseError::InvalidLength,
                NativeParseError::Invalid => AddressParseError::InvalidBase58,
            })?;
        Self::from_native(input, native)
    }
}

impl Address {
    #[must_use]
    pub fn from_bytes(bytes: [u8; solana_address::ADDRESS_BYTES]) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; solana_address::ADDRESS_BYTES] {
        self.0.as_array()
    }

    fn from_native(input: &str, native: NativeAddress) -> Result<Self, AddressParseError> {
        if native.to_string() != input {
            return Err(AddressParseError::NonCanonical);
        }
        Ok(Self(native))
    }
}

impl TryFrom<&BaseAddress> for Address {
    type Error = AddressParseError;

    fn try_from(address: &BaseAddress) -> Result<Self, Self::Error> {
        let bytes = address
            .as_bytes()
            .try_into()
            .map_err(|_| AddressParseError::InvalidLength)?;
        Ok(Self::from_bytes(bytes))
    }
}

impl From<&Address> for AddressText {
    fn from(address: &Address) -> Self {
        Self::new(AddressEncoding::Base58, address.to_string())
    }
}

impl TryFrom<&AddressText> for Address {
    type Error = AddressParseError;

    fn try_from(address: &AddressText) -> Result<Self, Self::Error> {
        if address.encoding != AddressEncoding::Base58 {
            return Err(AddressParseError::WrongEncoding);
        }
        address.text.parse()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressParseError {
    InvalidBase58,
    InvalidLength,
    NonCanonical,
    WrongEncoding,
}

impl fmt::Display for AddressParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidBase58 => "Solana address contains invalid Base58",
            Self::InvalidLength => "Solana address must contain exactly 32 bytes",
            Self::NonCanonical => "Solana address is not canonical Base58",
            Self::WrongEncoding => "Solana addresses use plain Base58 encoding",
        })
    }
}

impl Error for AddressParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_address_bytes() {
        let bytes = [7; solana_address::ADDRESS_BYTES];
        assert_eq!(Address::from_bytes(bytes).as_bytes(), &bytes);
    }

    #[test]
    fn renders_every_byte_as_canonical_plain_base58() {
        for bytes in [
            [0; solana_address::ADDRESS_BYTES],
            [7; solana_address::ADDRESS_BYTES],
            [u8::MAX; solana_address::ADDRESS_BYTES],
        ] {
            let address = Address::from_bytes(bytes);
            let rendered = address.to_string();

            assert_eq!(rendered, NativeAddress::from(bytes).to_string());
            assert_eq!(
                rendered
                    .parse::<Address>()
                    .expect("canonical rendering must parse"),
                address
            );
        }
    }

    #[test]
    fn round_trips_the_protocol_neutral_address_bytes() {
        let address = Address::from_bytes([23; solana_address::ADDRESS_BYTES]);
        let portable = address.address();

        assert_eq!(portable.as_bytes(), address.as_bytes());
        assert_eq!(
            Address::try_from(&portable).expect("32 portable bytes must convert"),
            address
        );
        assert_eq!(
            Address::try_from(&BaseAddress::from([0; 31])),
            Err(AddressParseError::InvalidLength)
        );
    }

    #[test]
    fn uses_plain_base58_and_rejects_cross_codec_tags() {
        let address = Address::from_bytes([31; solana_address::ADDRESS_BYTES]);
        let text = AddressText::from(&address);

        assert_eq!(text.encoding, AddressEncoding::Base58);
        assert_eq!(text.text, address.to_string());
        assert_eq!(
            Address::try_from(&text).expect("plain Base58 text must convert"),
            address
        );

        for encoding in [
            AddressEncoding::Base58Check,
            AddressEncoding::Bech32,
            AddressEncoding::Bech32m,
            AddressEncoding::Hex,
        ] {
            let mislabeled = AddressText::new(encoding, address.to_string());
            assert_eq!(
                Address::try_from(&mislabeled),
                Err(AddressParseError::WrongEncoding)
            );
        }
    }

    #[test]
    fn parses_canonical_minimum_and_maximum_values() {
        let minimum = "11111111111111111111111111111111"
            .parse::<Address>()
            .expect("canonical zero bytes must parse");
        assert_eq!(minimum.as_bytes(), &[0; 32]);

        let maximum_bytes = [u8::MAX; solana_address::ADDRESS_BYTES];
        let maximum_text = NativeAddress::from(maximum_bytes).to_string();
        assert_eq!(maximum_text.len(), 44);
        let maximum = maximum_text
            .parse::<Address>()
            .expect("canonical maximum bytes must parse");
        assert_eq!(maximum.as_bytes(), &maximum_bytes);
    }

    #[test]
    fn rejects_invalid_base58_alphabet() {
        for input in [
            "0",
            "O1111111111111111111111111111111",
            "I1111111111111111111111111111111",
            "l1111111111111111111111111111111",
            " 11111111111111111111111111111111",
        ] {
            assert_eq!(
                input.parse::<Address>(),
                Err(AddressParseError::InvalidBase58)
            );
        }
    }

    #[test]
    fn rejects_empty_and_wrong_length_values() {
        for input in [
            "",
            "1111111111111111111111111111111",
            "111111111111111111111111111111111",
            "111111111111111111111111111111111111111111111",
        ] {
            assert_eq!(
                input.parse::<Address>(),
                Err(AddressParseError::InvalidLength)
            );
        }
    }

    #[test]
    fn rejects_a_decode_reencode_mismatch() {
        let input = "11111111111111111111111111111111";
        let different_value = NativeAddress::from([1; solana_address::ADDRESS_BYTES]);
        assert_eq!(
            Address::from_native(input, different_value),
            Err(AddressParseError::NonCanonical)
        );
    }

    #[test]
    fn accepts_canonical_off_curve_values() {
        let native = (0..=u8::MAX)
            .map(|byte| NativeAddress::from([byte; solana_address::ADDRESS_BYTES]))
            .find(|address| !address.is_on_curve())
            .expect("the deterministic fixture range must contain an off-curve address");
        let input = native.to_string();
        let parsed = input
            .parse::<Address>()
            .expect("canonical off-curve addresses remain readable");

        assert_eq!(parsed.as_bytes(), native.as_array());
        assert!(!NativeAddress::from(*parsed.as_bytes()).is_on_curve());
    }
}
