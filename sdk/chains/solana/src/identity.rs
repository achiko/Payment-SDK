use indexing::{AssetId, ChainId, IndexScope};

use crate::{Error, ErrorKind, Lamport, SOL};

const NATIVE: &str = "native";

/// Solana asset behavior supported by the current native-only integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Native,
}

impl AssetKind {
    pub(crate) fn id(self) -> AssetId {
        match self {
            Self::Native => AssetId {
                chain: ChainId(crate::CHAIN.to_owned()),
                asset: NATIVE.to_owned(),
            },
        }
    }

    pub(crate) const fn metadata(self) -> &'static base::Asset {
        match self {
            Self::Native => &SOL,
        }
    }

    pub(crate) fn display(self, amount: Lamport) -> base::Decimal {
        self.metadata().from_atomic(amount.atomic().into())
    }
}

/// One supported Solana asset bound to one configured runtime network scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletConfig {
    scope: IndexScope,
    asset: AssetKind,
}

impl WalletConfig {
    pub fn new(network: impl Into<String>, asset: AssetKind) -> Result<Self, Error> {
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
            asset,
        })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    #[must_use]
    pub const fn asset(&self) -> AssetKind {
        self.asset
    }

    #[must_use]
    pub fn id(&self) -> AssetId {
        self.asset.id()
    }

    #[must_use]
    pub fn display(&self, amount: Lamport) -> base::Decimal {
        self.asset.display(amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_one_scope_and_native_asset_identity() {
        let config =
            WalletConfig::new("localnet", AssetKind::Native).expect("network slug must be valid");

        assert_eq!(config.scope().chain, ChainId("solana".to_owned()));
        assert_eq!(config.scope().network, "localnet");
        assert_eq!(config.asset(), AssetKind::Native);
        assert_eq!(
            config.id(),
            AssetId {
                chain: ChainId("solana".to_owned()),
                asset: "native".to_owned(),
            }
        );
        assert_eq!(
            config.display(Lamport::from_atomic(1)).to_string(),
            "0.000000001"
        );
        assert_eq!(config.asset().metadata(), &SOL);
    }

    #[test]
    fn rejects_an_empty_network_without_adding_an_spl_identity() {
        assert_eq!(
            WalletConfig::new(" ", AssetKind::Native)
                .expect_err("blank network must fail")
                .kind(),
            ErrorKind::InvalidIdentity
        );
    }
}
