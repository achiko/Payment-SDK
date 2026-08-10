use std::time::Duration;

use chain_identity::AssetId;
use deposits::PolicyIdentity;
use indexing::IndexScope;

use crate::{bitcoin_policy::BitcoinPaymentPolicy, policy::PaymentPolicy};

/// The one chain-native policy bound to a PS process and database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivePaymentPolicy {
    Ethereum(PaymentPolicy),
    Bitcoin(BitcoinPaymentPolicy),
}

impl ActivePaymentPolicy {
    #[must_use]
    pub const fn scope(&self) -> &IndexScope {
        match self {
            Self::Ethereum(policy) => &policy.scope,
            Self::Bitcoin(policy) => &policy.scope,
        }
    }

    #[must_use]
    pub const fn deposit_ttl(&self) -> Duration {
        match self {
            Self::Ethereum(policy) => policy.deposit_ttl,
            Self::Bitcoin(policy) => policy.deposit_ttl,
        }
    }

    #[must_use]
    pub fn identity(&self) -> PolicyIdentity {
        match self {
            Self::Ethereum(policy) => policy.identity(),
            Self::Bitcoin(policy) => policy.identity(),
        }
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        match self {
            Self::Ethereum(policy) => policy.version,
            Self::Bitcoin(policy) => policy.version,
        }
    }

    #[must_use]
    pub fn digest_hex(&self) -> String {
        match self {
            Self::Ethereum(policy) => policy.digest_hex(),
            Self::Bitcoin(policy) => policy.digest_hex(),
        }
    }

    #[must_use]
    pub const fn ethereum_chain_id(&self) -> Option<u64> {
        match self {
            Self::Ethereum(policy) => Some(policy.ethereum_chain_id),
            Self::Bitcoin(_) => None,
        }
    }

    pub fn enabled_asset(&self, asset: &AssetId) -> bool {
        match self {
            Self::Ethereum(policy) => policy.asset(asset).is_ok(),
            Self::Bitcoin(policy) => &policy.asset == asset,
        }
    }
}

impl From<PaymentPolicy> for ActivePaymentPolicy {
    fn from(value: PaymentPolicy) -> Self {
        Self::Ethereum(value)
    }
}

impl From<BitcoinPaymentPolicy> for ActivePaymentPolicy {
    fn from(value: BitcoinPaymentPolicy) -> Self {
        Self::Bitcoin(value)
    }
}
