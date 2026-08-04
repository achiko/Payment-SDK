use crate::{EthereumAddress, EthereumAsset, Wei};
use signer::KeyLocator;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EthereumCollectionRequest {
    Native {
        from: EthereumAddress,
        key: KeyLocator,
        destination: EthereumAddress,
    },
    Token {
        token: EthereumAddress,
        from: EthereumAddress,
        key: KeyLocator,
        destination: EthereumAddress,
        /// `None` means query and sweep the complete token balance.
        amount: Option<Wei>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EthereumCollectionRequirement {
    NativeGasBalance {
        address: EthereumAddress,
        current: Wei,
        required: Wei,
        deficit: Wei,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumCollectionAttribution {
    pub address: EthereumAddress,
    pub asset: EthereumAsset,
    /// Token/native amount debited from the deposit, excluding separately observed gas.
    pub gross_debit: Wei,
}
