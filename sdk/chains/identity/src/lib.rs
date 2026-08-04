//! Opaque, transport-safe chain identifiers and atomic values.
//!
//! Concrete Bitcoin, Ethereum, and future chain-native types do not belong here.

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetId {
    pub chain: ChainId,
    /// Canonical chain-owned value, for example `native` or a token contract address.
    pub asset: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalAddress {
    pub chain: ChainId,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalTransactionId {
    pub chain: ChainId,
    pub value: String,
}

/// Unsigned integer in atomic units, encoded as a 256-bit big-endian magnitude.
/// Display precision belongs to asset metadata, never this value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AtomicAmount(pub [u8; 32]);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SignedAtomicAmount {
    pub negative: bool,
    pub magnitude: AtomicAmount,
}
