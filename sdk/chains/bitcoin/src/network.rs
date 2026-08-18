use base::{NetworkId, NetworkKind};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    Mainnet,
    Testnet3,
    Testnet4,
    Signet,
    Regtest,
}

impl Network {
    #[must_use]
    pub const fn id(self) -> NetworkId {
        match self {
            Self::Mainnet => NetworkId::new("bitcoin-mainnet", NetworkKind::Mainnet),
            Self::Testnet3 => NetworkId::new("bitcoin-testnet3", NetworkKind::Testnet),
            Self::Testnet4 => NetworkId::new("bitcoin-testnet4", NetworkKind::Testnet),
            Self::Signet => NetworkId::new("bitcoin-signet", NetworkKind::Testnet),
            Self::Regtest => NetworkId::new("bitcoin-regtest", NetworkKind::Testnet),
        }
    }
}
