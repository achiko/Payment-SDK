use crate::{BitcoinAddress, Satoshi};
use indexing::BlockHeight;
use signer::{KeyLocator, OperationId};
use transaction_utxo::FeeRate;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCollectionSource {
    pub address: BitcoinAddress,
    pub key: KeyLocator,
    pub birthday: BlockHeight,
}

/// One UTXO transaction can debit many deposit addresses and create one master output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinBatchCollectionRequest {
    pub signing_operation_id: OperationId,
    pub sources: Vec<BitcoinCollectionSource>,
    pub destination: BitcoinAddress,
    pub minimum_confirmations: u64,
    pub fee_rate: Option<FeeRate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitcoinCollectionRequirement {
    NoSpendableOutputs { address: BitcoinAddress },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinCollectionAttribution {
    pub address: BitcoinAddress,
    pub key: KeyLocator,
    /// Gross value removed from this deposit's inputs, before shared transaction fee.
    pub gross_input: Satoshi,
}
