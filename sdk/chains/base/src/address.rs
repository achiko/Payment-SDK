use std::{error::Error, fmt};

/// Portable classification for address validation failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressErrorKind {
    Empty,
    InvalidEncoding,
    InvalidFormat,
    InvalidLength,
    InvalidChecksum,
    WrongNetwork,
}

/// Protocol-neutral address validation error with safe context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressError {
    pub kind: AddressErrorKind,
    pub message: String,
}

impl AddressError {
    #[must_use]
    pub fn new(kind: AddressErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for AddressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AddressError {}

/// Validates the representation invariants owned by an address type.
pub trait AddressValidator {
    fn validate(&self) -> Result<(), AddressError>;
}

/// Opaque address bytes shared at protocol-neutral boundaries.
///
/// Parsing, network validation, checksums, and user-facing encodings stay in
/// concrete protocol crates. This value carries only the resulting bytes.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Address(Vec<u8>);

impl Address {
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl<const N: usize> From<[u8; N]> for Address {
    fn from(bytes: [u8; N]) -> Self {
        Self(bytes.into())
    }
}

impl From<Vec<u8>> for Address {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl AddressValidator for Address {
    fn validate(&self) -> Result<(), AddressError> {
        if self.0.is_empty() {
            return Err(AddressError::new(
                AddressErrorKind::Empty,
                "address bytes must not be empty",
            ));
        }
        Ok(())
    }
}

/// A value that can expose its protocol-neutral address bytes.
pub trait Addresser {
    fn address(&self) -> Address;
}

impl Addresser for Address {
    fn address(&self) -> Address {
        self.clone()
    }
}

impl<T: Addresser + ?Sized> Addresser for &T {
    fn address(&self) -> Address {
        (*self).address()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_is_an_opaque_displayable_byte_value() {
        let address = Address::from([0x01, 0xab, 0xff]);
        assert_eq!(address.as_bytes(), &[0x01, 0xab, 0xff]);
        assert_eq!(address.to_string(), "01abff");
        assert_eq!(address.address(), address);
    }
}
