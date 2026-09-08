use crate::{ChainError, Network, SpendSource};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    hashes::Hash, transaction::Version,
};

use super::{Input, Output, SighashType};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction {
    pub version: i32,
    pub lock_time: u32,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub sighash_type: SighashType,
}

impl UnsignedTransaction {
    pub(super) fn from_selected(utxos: Vec<SpendSource>, outputs: Vec<Output>) -> Self {
        Self {
            version: 2,
            lock_time: 0,
            inputs: utxos
                .into_iter()
                .map(|utxo| Input {
                    utxo,
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32(),
                })
                .collect(),
            outputs,
            sighash_type: SighashType::All,
        }
    }

    pub(super) fn native(&self, network: Network) -> Result<Transaction, ChainError> {
        let input = self
            .inputs
            .iter()
            .map(|input| TxIn {
                previous_output: OutPoint::new(
                    Txid::from_byte_array(input.utxo.transaction_id),
                    input.utxo.output_index,
                ),
                script_sig: ScriptBuf::new(),
                sequence: Sequence(input.sequence),
                witness: Witness::new(),
            })
            .collect();
        let output = self
            .outputs
            .iter()
            .map(|output| {
                Ok(TxOut {
                    value: Amount::from_sat(output.value.0),
                    script_pubkey: output.address.script_pubkey_for_network(network)?,
                })
            })
            .collect::<Result<Vec<_>, ChainError>>()?;
        Ok(Transaction {
            version: Version(self.version),
            lock_time: absolute::LockTime::from_consensus(self.lock_time),
            input,
            output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Address, ChainErrorKind, Satoshi, SpendSource};

    #[test]
    fn selected_funding_keeps_order_and_transaction_defaults() {
        let sources = [7, 3].map(|index| SpendSource {
            transaction_id: [index; 32],
            output_index: u32::from(index),
            value: Satoshi(u64::from(index)),
            script_pubkey: vec![0x51],
            satisfaction_weight: 109,
        });
        let outputs = [11, 5].map(|value| {
            Output::from_atomic(
                Address::from_encoded("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn"),
                Satoshi(value),
            )
        });
        let unsigned = UnsignedTransaction::from_selected(sources.to_vec(), outputs.to_vec());

        assert_eq!(unsigned.version, 2);
        assert_eq!(unsigned.lock_time, 0);
        assert_eq!(unsigned.sighash_type, SighashType::All);
        assert_eq!(unsigned.outputs, outputs);
        assert_eq!(unsigned.inputs.len(), sources.len());
        for (input, source) in unsigned.inputs.iter().zip(&sources) {
            assert_eq!(&input.utxo, source);
            assert_eq!(
                input.sequence,
                Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32()
            );
        }
    }

    #[test]
    fn native_conversion_keeps_unsigned_fields_and_empty_signing_material() {
        let address = Address::from_encoded("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn");
        let unsigned = UnsignedTransaction {
            version: -4,
            lock_time: u32::MAX,
            inputs: vec![Input {
                utxo: SpendSource {
                    transaction_id: [7; 32],
                    output_index: u32::MAX,
                    value: Satoshi(42),
                    script_pubkey: vec![0x51],
                    satisfaction_weight: 109,
                },
                sequence: 123,
            }],
            outputs: vec![Output::from_atomic(address.clone(), Satoshi(u64::MAX))],
            sighash_type: SighashType::All,
        };
        let native = unsigned.native(Network::Regtest).unwrap();
        assert_eq!(native.version, Version(-4));
        assert_eq!(native.lock_time.to_consensus_u32(), u32::MAX);
        assert_eq!(native.input.len(), 1);
        assert_eq!(
            native.input[0].previous_output,
            OutPoint::new(Txid::from_byte_array([7; 32]), u32::MAX)
        );
        assert_eq!(native.input[0].sequence, Sequence(123));
        assert!(native.input[0].script_sig.is_empty());
        assert!(native.input[0].witness.is_empty());
        assert_eq!(
            native.output,
            [TxOut {
                value: Amount::from_sat(u64::MAX),
                script_pubkey: address.script_pubkey_for_network(Network::Regtest).unwrap(),
            }]
        );
        assert_eq!(unsigned.inputs[0].utxo.value, Satoshi(42));
    }

    #[test]
    fn native_conversion_keeps_output_network_rejection() {
        let address = Address::from_encoded("1BitcoinEaterAddressDontSendf59kuE");
        let expected = address
            .script_pubkey_for_network(Network::Regtest)
            .unwrap_err();
        let unsigned = UnsignedTransaction {
            version: 2,
            lock_time: 0,
            inputs: Vec::new(),
            outputs: vec![Output::from_atomic(address, Satoshi(1_000))],
            sighash_type: SighashType::All,
        };
        assert_eq!(expected.kind, ChainErrorKind::InvalidAddress);
        assert_eq!(unsigned.native(Network::Regtest), Err(expected));
    }
}
