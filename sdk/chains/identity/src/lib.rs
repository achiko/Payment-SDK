//! Opaque, transport-safe chain identifiers and atomic values.
//!
//! Concrete Bitcoin, Ethereum, and future chain-native types do not belong here.

mod amount;

pub use amount::{
    AtomicAmount, AtomicAmountArithmeticError, AtomicAmountParseError, SignedAtomicAmount,
};

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
