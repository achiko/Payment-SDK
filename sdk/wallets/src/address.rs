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
