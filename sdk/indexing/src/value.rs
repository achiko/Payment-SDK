#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexScope {
    pub chain: ChainId,
    /// Chain-owned canonical network name, such as mainnet, sepolia, or regtest.
    pub network: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetId {
    pub chain: ChainId,
    pub asset: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalAddress {
    /// The complete chain/network namespace in which `value` is canonical.
    pub scope: crate::IndexScope,
    pub value: String,
}

impl CanonicalAddress {
    #[must_use]
    pub fn belongs_to(&self, scope: &crate::IndexScope) -> bool {
        &self.scope == scope
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionRef {
    /// The complete chain/network namespace in which `value` is canonical.
    pub scope: crate::IndexScope,
    pub value: String,
}

impl TransactionRef {
    #[must_use]
    pub fn belongs_to(&self, scope: &crate::IndexScope) -> bool {
        &self.scope == scope
    }
}
