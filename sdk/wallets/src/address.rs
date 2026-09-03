use base::Address;

use crate::Error;

/// Canonical user-facing address at a transport boundary.
///
/// The protocol-neutral `Address` remains opaque bytes. A concrete wallet
/// owns the reversible conversion for its configured chain and network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressText {
    pub encoding: AddressEncoding,
    pub text: String,
}

impl AddressText {
    #[must_use]
    pub fn new(encoding: AddressEncoding, text: impl Into<String>) -> Self {
        Self {
            encoding,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressEncoding {
    Base58,
    Base58Check,
    Bech32,
    Bech32m,
    Hex,
}

/// Formats and parses addresses using one wallet's chain and network rules.
pub trait AddressFormat: Send + Sync {
    fn address_text(&self, address: &Address) -> Result<AddressText, Error>;

    fn parse_address(&self, address: &AddressText) -> Result<Address, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_base58_is_distinct_from_base58check() {
        assert_ne!(AddressEncoding::Base58, AddressEncoding::Base58Check);

        let address = AddressText::new(AddressEncoding::Base58, "plain-base58");
        assert_eq!(address.encoding, AddressEncoding::Base58);
    }
}
