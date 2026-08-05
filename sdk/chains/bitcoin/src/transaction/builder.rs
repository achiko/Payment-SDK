use crate::{BitcoinAddress, Satoshi};
use signer::OperationId;
use transaction_utxo::{Amount, FeeRate, Utxo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinUtxo {
    pub transaction_id: [u8; 32],
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub satisfaction_weight: u64,
    pub key: signer::KeyLocator,
}

impl Utxo for BitcoinUtxo {
    type Id = ([u8; 32], u32);

    fn id(&self) -> Self::Id {
        (self.transaction_id, self.output_index)
    }

    fn value(&self) -> Amount {
        Amount(u128::from(self.value.0))
    }

    fn satisfaction_weight(&self) -> u64 {
        self.satisfaction_weight
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinInput {
    pub utxo: BitcoinUtxo,
    pub sequence: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinOutput {
    pub address: BitcoinAddress,
    pub value: Satoshi,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinBuildRequest {
    pub signing_operation_id: OperationId,
    pub available: Vec<BitcoinUtxo>,
    pub recipients: Vec<BitcoinOutput>,
    pub change_address: BitcoinAddress,
    pub fee_rate: FeeRate,
    pub drain_wallet: bool,
}

pub trait BitcoinTransactionBuilder: Send + Sync {
    fn build(
        &self,
        request: BitcoinBuildRequest,
    ) -> Result<super::UnsignedBitcoinTransaction, chain_contract::ChainError>;
}
