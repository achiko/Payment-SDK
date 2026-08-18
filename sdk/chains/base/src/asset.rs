use num_bigint::BigUint;

use crate::{Chain, Decimal, DecimalError};

/// Metadata describing an asset on one concrete blockchain network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Asset<R: 'static = &'static str> {
    pub chain: Chain<R>,
    pub name: &'static str,
    pub ticker: &'static str,
    pub decimals: u32,
}

impl<R: 'static> Asset<R> {
    #[must_use]
    pub const fn new(
        chain: Chain<R>,
        name: &'static str,
        ticker: &'static str,
        decimals: u32,
    ) -> Self {
        Self {
            chain,
            name,
            ticker,
            decimals,
        }
    }

    pub fn to_atomic(&self, amount: &Decimal) -> Result<BigUint, DecimalError> {
        amount.to_atomic(self.decimals)
    }

    #[must_use]
    pub fn from_atomic(&self, amount: BigUint) -> Decimal {
        Decimal::from_atomic(amount, self.decimals)
    }
}

/// Minimal asset metadata exposed by chains and concrete assets.
///
/// The network identifier is available from `chain().network_id`, keeping the
/// trait within the repository's three-method interface limit.
pub trait Asseter {
    type NetworkIdStorage: 'static;

    fn chain(&self) -> &Chain<Self::NetworkIdStorage>;
    fn name(&self) -> &str;
    fn ticker(&self) -> &str;
}

impl<R: 'static> Asseter for Chain<R> {
    type NetworkIdStorage = R;

    fn chain(&self) -> &Chain<R> {
        self
    }

    fn name(&self) -> &str {
        self.name
    }

    fn ticker(&self) -> &str {
        self.ticker
    }
}

impl<R: 'static> Asseter for Asset<R> {
    type NetworkIdStorage = R;

    fn chain(&self) -> &Chain<R> {
        &self.chain
    }

    fn name(&self) -> &str {
        self.name
    }

    fn ticker(&self) -> &str {
        self.ticker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NetworkId, NetworkKind};

    #[test]
    fn chain_and_asset_expose_the_same_minimal_contract() {
        let chain = Chain::new(NetworkId::new(1_u64, NetworkKind::Mainnet), "example", "EX");
        let asset = Asset::new(chain, "Example USD", "xUSD", 6);

        assert_eq!(chain.name(), "example");
        assert_eq!(asset.name(), "Example USD");
        assert_eq!(asset.ticker(), "xUSD");
        assert_eq!(asset.decimals, 6);
        assert_eq!(asset.chain().network_id.value(), &1);
    }
}
