use crate::{EthereumAddress, EthereumAsset, EthereumSignedTransaction, Wei};
use signer::{KeyLocator, OperationId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EthereumCollectionRequest {
    Native {
        signing_operation_id: OperationId,
        from: EthereumAddress,
        key: KeyLocator,
        destination: EthereumAddress,
    },
    Token {
        signing_operation_id: OperationId,
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

/// One fully signed collection attempt that has not been broadcast.
///
/// The caller may persist the opaque signed envelope before submission. No
/// durable workflow or retry state is retained by the Wallet Service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumPreparedCollection {
    pub transaction: EthereumSignedTransaction,
    pub attribution: Vec<EthereumCollectionAttribution>,
}
