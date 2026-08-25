use serde::{Deserialize, Serialize};
use utoipa::IntoParams;

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Chain {
    Bitcoin,
    Ethereum,
}

impl Chain {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bitcoin => "bitcoin",
            Self::Ethereum => "ethereum",
        }
    }
}

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WalletAsset {
    Btc,
    Eth,
    Usdc,
}

impl WalletAsset {
    #[must_use]
    pub const fn chain(self) -> Chain {
        match self {
            Self::Btc => Chain::Bitcoin,
            Self::Eth | Self::Usdc => Chain::Ethereum,
        }
    }
}

/// Public wallet fields returned by both wallet creation and lookup.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Wallet {
    pub id: String,
    pub asset: WalletAsset,
    pub chain: Chain,
    pub network: String,
    pub address: String,
}

impl From<wallets::WalletInfo<String, WalletAsset>> for Wallet {
    fn from(value: wallets::WalletInfo<String, WalletAsset>) -> Self {
        let asset = value.family;
        Self {
            id: value.id,
            asset,
            chain: asset.chain(),
            network: value.scope.network,
            address: value.address.text,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
pub struct WalletPath {
    pub id: String,
}

/// A chain-native address accepted by both transaction submission endpoints.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddressInput {
    pub encoding: AddressEncoding,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddressEncoding {
    Base58Check,
    Bech32,
    Bech32m,
    Hex,
}

impl From<AddressInput> for wallets::AddressText {
    fn from(address: AddressInput) -> Self {
        let encoding = match address.encoding {
            AddressEncoding::Base58Check => wallets::AddressEncoding::Base58Check,
            AddressEncoding::Bech32 => wallets::AddressEncoding::Bech32,
            AddressEncoding::Bech32m => wallets::AddressEncoding::Bech32m,
            AddressEncoding::Hex => wallets::AddressEncoding::Hex,
        };
        Self::new(encoding, address.text)
    }
}
