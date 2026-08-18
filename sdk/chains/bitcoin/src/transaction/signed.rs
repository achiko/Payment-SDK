use crate::{ChainError, ChainErrorKind};
use bitcoin::{Transaction, Txid, consensus, hashes::Hash};
use std::{fmt, str::FromStr};

use crate::{Outpoint, Satoshi};

/// A non-witness transaction ID stored in the digest byte order used by
/// `rust-bitcoin`. Text formatting and parsing use Bitcoin Core's conventional
/// reversed hexadecimal display order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub [u8; 32]);

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        Txid::from_byte_array(self.0).fmt(formatter)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for Id {
    type Err = bitcoin::hex::HexToArrayError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<Txid>().map(Self::from)
    }
}

impl From<Txid> for Id {
    fn from(id: Txid) -> Self {
        Self(id.to_byte_array())
    }
}

impl From<Id> for Txid {
    fn from(id: Id) -> Self {
        Self::from_byte_array(id.0)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SignedTransaction {
    id: Id,
    consensus_bytes: Vec<u8>,
}

/// Ordered, chain-native fields decoded from one signed Bitcoin transaction.
///
/// Input values and the transaction fee are deliberately absent: neither can
/// be derived from the spending transaction without resolving every previous
/// output. A caller such as Payment Service must compare `inputs` with its
/// independently reserved previous-output facts before computing a fee.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    pub transaction_id: Id,
    pub version: i32,
    pub lock_time: u32,
    pub virtual_size: u64,
    pub inputs: Vec<InputInspection>,
    pub outputs: Vec<OutputInspection>,
}

/// One signed transaction input in consensus order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputInspection {
    pub outpoint: Outpoint,
    pub sequence: u32,
}

/// One signed transaction output in consensus order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInspection {
    pub output_index: u32,
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
}

impl SignedTransaction {
    /// Decodes exact Bitcoin consensus bytes and verifies their non-witness
    /// transaction ID before accepting them at the signed-transaction boundary.
    pub fn from_consensus_bytes(
        expected_id: Id,
        consensus_bytes: Vec<u8>,
    ) -> Result<Self, ChainError> {
        let transaction: Transaction =
            consensus::deserialize(&consensus_bytes).map_err(|error| {
                invalid_transaction(format!(
                    "could not decode signed Bitcoin consensus bytes: {error}"
                ))
            })?;
        let computed_id = Id::from(transaction.compute_txid());
        if computed_id != expected_id {
            return Err(invalid_transaction(format!(
                "signed Bitcoin transaction ID mismatch: expected {expected_id}, computed {computed_id}"
            )));
        }
        Ok(Self {
            id: expected_id,
            consensus_bytes,
        })
    }

    #[must_use]
    pub const fn id(&self) -> Id {
        self.id
    }

    #[must_use]
    pub fn consensus_bytes(&self) -> &[u8] {
        &self.consensus_bytes
    }

    #[must_use]
    pub fn into_consensus_bytes(self) -> Vec<u8> {
        self.consensus_bytes
    }

    #[must_use]
    pub fn into_parts(self) -> (Id, Vec<u8>) {
        (self.id, self.consensus_bytes)
    }

    /// Decodes the retained consensus bytes and returns BIP141 virtual bytes.
    pub fn virtual_size(&self) -> Result<u64, ChainError> {
        let transaction = decode_signed_transaction(&self.consensus_bytes)?;
        u64::try_from(transaction.vsize())
            .map_err(|_| invalid_transaction("signed Bitcoin virtual size exceeds u64"))
    }

    /// Decodes the retained exact consensus bytes into reviewable transaction
    /// structure without inventing previous-output values or a fee.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transaction error if the retained bytes no longer
    /// decode, their txid differs from the verified boundary value, or a
    /// platform-sized count cannot be represented by the public integer types.
    pub fn inspect(&self) -> Result<Inspection, ChainError> {
        let transaction = decode_signed_transaction(&self.consensus_bytes)?;
        let transaction_id = Id::from(transaction.compute_txid());
        if transaction_id != self.id {
            return Err(invalid_transaction(format!(
                "signed Bitcoin transaction ID mismatch: expected {}, computed {transaction_id}",
                self.id
            )));
        }
        let virtual_size = u64::try_from(transaction.vsize())
            .map_err(|_| invalid_transaction("signed Bitcoin virtual size exceeds u64"))?;
        let inputs = transaction
            .input
            .iter()
            .map(|input| InputInspection {
                outpoint: Outpoint {
                    transaction_id: Id::from(input.previous_output.txid),
                    output_index: input.previous_output.vout,
                },
                sequence: input.sequence.to_consensus_u32(),
            })
            .collect();
        let outputs = transaction
            .output
            .iter()
            .enumerate()
            .map(|(output_index, output)| {
                Ok(OutputInspection {
                    output_index: u32::try_from(output_index).map_err(|_| {
                        invalid_transaction("signed Bitcoin output index exceeds u32")
                    })?,
                    value: Satoshi(output.value.to_sat()),
                    script_pubkey: output.script_pubkey.as_bytes().to_vec(),
                })
            })
            .collect::<Result<Vec<_>, ChainError>>()?;
        Ok(Inspection {
            transaction_id,
            version: transaction.version.0,
            lock_time: transaction.lock_time.to_consensus_u32(),
            virtual_size,
            inputs,
            outputs,
        })
    }
}

impl fmt::Debug for SignedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedTransaction")
            .field("id", &self.id)
            .field(
                "consensus_bytes",
                &RedactedBytes(self.consensus_bytes.len()),
            )
            .finish()
    }
}

struct RedactedBytes(usize);

impl fmt::Debug for RedactedBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<redacted: {} bytes>", self.0)
    }
}

fn invalid_transaction(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

fn decode_signed_transaction(consensus_bytes: &[u8]) -> Result<Transaction, ChainError> {
    consensus::deserialize(consensus_bytes).map_err(|error| {
        invalid_transaction(format!(
            "could not decode signed Bitcoin consensus bytes: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness, absolute, transaction::Version,
    };

    fn transaction() -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::from_consensus(500_000),
            input: vec![
                TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([7; 32]), 3),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::from_slice(&[b"signed-witness"]),
                },
                TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([8; 32]), 9),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
            ],
            output: vec![
                TxOut {
                    value: Amount::from_sat(42_000),
                    script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
                },
                TxOut {
                    value: Amount::from_sat(7_000),
                    script_pubkey: ScriptBuf::from_bytes(vec![0x00, 0x14, 1, 2, 3]),
                },
            ],
        }
    }

    #[test]
    fn transaction_id_uses_conventional_display_byte_order() {
        let displayed = format!("{:064x}", 1);
        let id: Id = displayed
            .parse()
            .expect("canonical transaction ID should parse");
        let mut internal = [0_u8; 32];
        internal[0] = 1;

        assert_eq!(id.0, internal);
        assert_eq!(id.to_string(), displayed);
        assert_eq!(format!("{id:?}"), displayed);
    }

    #[test]
    fn transaction_id_rejects_invalid_hex() {
        assert!("not-a-transaction-id".parse::<Id>().is_err());
        assert!("00".parse::<Id>().is_err());
    }

    #[test]
    fn signed_transaction_preserves_verified_exact_bytes() {
        let transaction = transaction();
        let bytes = consensus::serialize(&transaction);
        let expected_id = Id::from(transaction.compute_txid());
        let signed = SignedTransaction::from_consensus_bytes(expected_id, bytes.clone())
            .expect("matching signed transaction bytes should be accepted");

        assert_eq!(signed.id(), expected_id);
        assert_eq!(signed.consensus_bytes(), bytes);
    }

    #[test]
    fn signed_transaction_rejects_malformed_bytes() {
        let error = SignedTransaction::from_consensus_bytes(Id([0; 32]), vec![0xff])
            .expect_err("malformed consensus bytes must fail");

        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
        assert!(error.message.contains("could not decode"));
    }

    #[test]
    fn signed_transaction_rejects_mismatched_transaction_id() {
        let bytes = consensus::serialize(&transaction());
        let error = SignedTransaction::from_consensus_bytes(Id([0; 32]), bytes)
            .expect_err("a mismatched transaction ID must fail");

        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
        assert!(error.message.contains("transaction ID mismatch"));
    }

    #[test]
    fn signed_transaction_debug_redacts_consensus_bytes() {
        let transaction = transaction();
        let signed = SignedTransaction::from_consensus_bytes(
            Id::from(transaction.compute_txid()),
            consensus::serialize(&transaction),
        )
        .expect("matching signed transaction bytes should be accepted");
        let debug = format!("{signed:?}");

        assert!(debug.contains("consensus_bytes: <redacted:"));
        assert!(!debug.contains("consensus_bytes: ["));
        assert!(!debug.contains("signed-witness"));
    }

    #[test]
    fn signed_transaction_reports_consensus_virtual_size() {
        let transaction = transaction();
        let signed = SignedTransaction::from_consensus_bytes(
            Id::from(transaction.compute_txid()),
            consensus::serialize(&transaction),
        )
        .expect("matching signed transaction bytes should be accepted");

        assert_eq!(
            signed.virtual_size().expect("valid bytes have a vsize"),
            u64::try_from(transaction.vsize()).expect("test vsize fits u64")
        );
    }

    #[test]
    fn signed_transaction_inspection_preserves_ordered_consensus_fields() {
        let transaction = transaction();
        let signed = SignedTransaction::from_consensus_bytes(
            Id::from(transaction.compute_txid()),
            consensus::serialize(&transaction),
        )
        .expect("matching signed transaction bytes should be accepted");

        let inspection = signed.inspect().expect("valid bytes should inspect");

        assert_eq!(inspection.transaction_id, signed.id());
        assert_eq!(inspection.version, Version::TWO.0);
        assert_eq!(inspection.lock_time, 500_000);
        assert_eq!(
            inspection.virtual_size,
            u64::try_from(transaction.vsize()).expect("test vsize fits u64")
        );
        assert_eq!(
            inspection.inputs,
            vec![
                InputInspection {
                    outpoint: Outpoint {
                        transaction_id: Id([7; 32]),
                        output_index: 3,
                    },
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32(),
                },
                InputInspection {
                    outpoint: Outpoint {
                        transaction_id: Id([8; 32]),
                        output_index: 9,
                    },
                    sequence: Sequence::MAX.to_consensus_u32(),
                },
            ]
        );
        assert_eq!(
            inspection.outputs,
            vec![
                OutputInspection {
                    output_index: 0,
                    value: Satoshi(42_000),
                    script_pubkey: vec![0x51],
                },
                OutputInspection {
                    output_index: 1,
                    value: Satoshi(7_000),
                    script_pubkey: vec![0x00, 0x14, 1, 2, 3],
                },
            ]
        );
    }
}
