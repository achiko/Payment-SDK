use bitcoin::ScriptBuf;
use chain_contract::{ChainError, ChainErrorKind};

use crate::{BitcoinAddress, BitcoinNetwork, BitcoinTransactionId, Satoshi};
use signer::{KeyLocator, OperationId};
use transaction_utxo::{Amount, Utxo};

const P2WPKH_SATISFACTION_WEIGHT: u64 = 109;
const P2TR_SATISFACTION_WEIGHT: u64 = 67;

/// A Bitcoin fee rate expressed as satoshis per 1,000 virtual bytes.
///
/// Weight units and virtual bytes are deliberately not interchangeable; fee
/// calculation converts transaction weight to virtual size before applying
/// this rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SatoshisPerKvb(u64);

impl SatoshisPerKvb {
    #[must_use]
    pub const fn new(satoshis: u64) -> Self {
        Self(satoshis)
    }

    #[must_use]
    pub const fn satoshis_per_kvb(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinUtxo {
    pub transaction_id: [u8; 32],
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
    pub satisfaction_weight: u64,
    pub key: signer::KeyLocator,
}

impl BitcoinUtxo {
    /// Accepts one exact PS-reserved/IX-sourced outpoint while deriving all
    /// signing weight from the verified chain-native script.
    pub fn from_exact_selection(
        network: BitcoinNetwork,
        address: &BitcoinAddress,
        key: KeyLocator,
        transaction_id: BitcoinTransactionId,
        output_index: u32,
        value: Satoshi,
        script_pubkey: Vec<u8>,
    ) -> Result<Self, ChainError> {
        let expected = address.script_pubkey_for_network(network)?;
        let script = ScriptBuf::from_bytes(script_pubkey);
        if script != expected {
            return Err(invalid_selection(
                "Bitcoin selected output script does not match its address",
            ));
        }
        let satisfaction_weight = if script.is_p2wpkh() {
            P2WPKH_SATISFACTION_WEIGHT
        } else if script.is_p2tr() {
            P2TR_SATISFACTION_WEIGHT
        } else {
            return Err(invalid_selection(
                "Bitcoin selected output must be P2WPKH or P2TR",
            ));
        };
        Ok(Self {
            transaction_id: transaction_id.0,
            output_index,
            value,
            script_pubkey: script.into_bytes(),
            satisfaction_weight,
            key,
        })
    }
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
    pub fee_rate: SatoshisPerKvb,
    pub drain_wallet: bool,
}

pub trait BitcoinTransactionBuilder: Send + Sync {
    fn build(
        &self,
        request: BitcoinBuildRequest,
    ) -> Result<super::UnsignedBitcoinTransaction, chain_contract::ChainError>;
}

fn invalid_selection(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{Address, CompressedPublicKey, PublicKey};

    use super::*;

    fn address_and_script() -> (BitcoinAddress, Vec<u8>) {
        let public_key = PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        let address = Address::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            bitcoin::Network::Regtest,
        );
        (
            BitcoinAddress(address.to_string()),
            address.script_pubkey().into_bytes(),
        )
    }

    #[test]
    fn exact_selection_derives_weight_from_verified_script() {
        let (address, script) = address_and_script();

        let utxo = BitcoinUtxo::from_exact_selection(
            BitcoinNetwork::Regtest,
            &address,
            KeyLocator::Identifier("opaque-key".to_owned()),
            BitcoinTransactionId([7; 32]),
            2,
            Satoshi(42_000),
            script,
        )
        .expect("matching P2WPKH selection must be accepted");

        assert_eq!(utxo.satisfaction_weight, P2WPKH_SATISFACTION_WEIGHT);
    }

    #[test]
    fn exact_selection_rejects_client_script_mismatch() {
        let (address, _) = address_and_script();

        let error = BitcoinUtxo::from_exact_selection(
            BitcoinNetwork::Regtest,
            &address,
            KeyLocator::Identifier("opaque-key".to_owned()),
            BitcoinTransactionId([7; 32]),
            2,
            Satoshi(42_000),
            vec![0x51],
        )
        .expect_err("mismatched selection script must fail");

        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
    }
}
