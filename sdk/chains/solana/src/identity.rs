use indexing::{AssetId, ChainId, IndexScope};

use crate::{Error, ErrorKind, Lamport};

const NATIVE: &str = "native";

/// Native SOL identity bound to one configured Solana network slug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeAsset {
    scope: IndexScope,
}

impl NativeAsset {
    pub fn new(network: impl Into<String>) -> Result<Self, Error> {
        let network = network.into();
        if network.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidIdentity,
                "Solana network slug must not be empty",
            ));
        }
        Ok(Self {
            scope: IndexScope {
                chain: ChainId(crate::CHAIN.to_owned()),
                network,
            },
        })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    #[must_use]
    pub fn id(&self) -> AssetId {
        AssetId {
            chain: self.scope.chain.clone(),
            asset: NATIVE.to_owned(),
        }
    }

    #[must_use]
    pub fn display(&self, amount: Lamport) -> base::Decimal {
        amount.decimal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_one_scope_and_native_asset_identity() {
        let asset = NativeAsset::new("localnet").expect("network slug must be valid");

        assert_eq!(asset.scope().chain, ChainId("solana".to_owned()));
        assert_eq!(asset.scope().network, "localnet");
        assert_eq!(
            asset.id(),
            AssetId {
                chain: ChainId("solana".to_owned()),
                asset: "native".to_owned(),
            }
        );
        assert_eq!(
            asset.display(Lamport::from_atomic(1)).to_string(),
            "0.000000001"
        );
    }

    #[test]
    fn rejects_an_empty_network_without_adding_an_spl_identity() {
        assert_eq!(
            NativeAsset::new(" ")
                .expect_err("blank network must fail")
                .kind(),
            ErrorKind::InvalidIdentity
        );
    }
}
