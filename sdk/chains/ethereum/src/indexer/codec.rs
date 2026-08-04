use indexing::{IndexError, IndexErrorKind, IndexRecordCodec};

use super::{EthereumUndo, EthereumWatchTarget};
use crate::{EthereumAddress, EthereumTransactionId};

const MAGIC: &[u8; 4] = b"ETHI";
const VERSION_V1: u8 = 1;
const HEADER_LENGTH: usize = MAGIC.len() + 2;

const TARGET_RECORD: u8 = 1;
const UNDO_RECORD: u8 = 2;

const ADDRESS_TARGET: u8 = 1;
const TRANSACTION_TARGET: u8 = 2;

/// Stable, explicitly versioned persistence encoding for Ethereum IX records.
///
/// This codec deliberately does not serialize the Rust enum/struct layout. A
/// persisted record therefore remains readable when implementation details or
/// dependency versions change.
#[derive(Clone, Copy, Debug, Default)]
pub struct EthereumIndexRecordCodec;

impl IndexRecordCodec for EthereumIndexRecordCodec {
    type Target = EthereumWatchTarget;
    type Undo = EthereumUndo;

    fn encode_target(&self, target: &Self::Target) -> Result<Vec<u8>, IndexError> {
        let mut encoded = Vec::with_capacity(HEADER_LENGTH + 1 + 32);
        write_header(&mut encoded, TARGET_RECORD);

        match target {
            EthereumWatchTarget::Address(address) => {
                encoded.push(ADDRESS_TARGET);
                encoded.extend_from_slice(&address.0);
            }
            EthereumWatchTarget::Transaction(transaction_id) => {
                encoded.push(TRANSACTION_TARGET);
                encoded.extend_from_slice(&transaction_id.0);
            }
        }

        Ok(encoded)
    }

    fn decode_target(&self, encoded: &[u8]) -> Result<Self::Target, IndexError> {
        let payload = read_payload(encoded, TARGET_RECORD)?;
        let Some((&target_kind, value)) = payload.split_first() else {
            return Err(codec_error("Ethereum IX target record has no target kind"));
        };

        match target_kind {
            ADDRESS_TARGET if value.len() == 20 => {
                let mut address = [0_u8; 20];
                address.copy_from_slice(value);
                Ok(EthereumWatchTarget::Address(EthereumAddress(address)))
            }
            TRANSACTION_TARGET if value.len() == 32 => {
                let mut transaction_id = [0_u8; 32];
                transaction_id.copy_from_slice(value);
                Ok(EthereumWatchTarget::Transaction(EthereumTransactionId(
                    transaction_id,
                )))
            }
            ADDRESS_TARGET => Err(codec_error(
                "Ethereum IX address target has an invalid encoded length",
            )),
            TRANSACTION_TARGET => Err(codec_error(
                "Ethereum IX transaction target has an invalid encoded length",
            )),
            _ => Err(codec_error("Ethereum IX target kind is not supported")),
        }
    }

    fn encode_undo(&self, undo: &Self::Undo) -> Result<Vec<u8>, IndexError> {
        let count = u32::try_from(undo.affected_transactions.len()).map_err(|_| {
            codec_error("Ethereum IX undo record contains too many transaction identifiers")
        })?;
        let transaction_bytes = undo
            .affected_transactions
            .len()
            .checked_mul(32)
            .ok_or_else(|| codec_error("Ethereum IX undo record length overflowed"))?;
        let capacity = HEADER_LENGTH
            .checked_add(4)
            .and_then(|length| length.checked_add(transaction_bytes))
            .ok_or_else(|| codec_error("Ethereum IX undo record length overflowed"))?;

        let mut encoded = Vec::with_capacity(capacity);
        write_header(&mut encoded, UNDO_RECORD);
        encoded.extend_from_slice(&count.to_be_bytes());
        for transaction_id in &undo.affected_transactions {
            encoded.extend_from_slice(&transaction_id.0);
        }
        Ok(encoded)
    }

    fn decode_undo(&self, encoded: &[u8]) -> Result<Self::Undo, IndexError> {
        let payload = read_payload(encoded, UNDO_RECORD)?;
        let Some(count_bytes) = payload.get(..4) else {
            return Err(codec_error("Ethereum IX undo record has no item count"));
        };
        let mut count_array = [0_u8; 4];
        count_array.copy_from_slice(count_bytes);
        let count = usize::try_from(u32::from_be_bytes(count_array)).map_err(|_| {
            codec_error("Ethereum IX undo item count is unsupported on this platform")
        })?;
        let transaction_bytes = count
            .checked_mul(32)
            .ok_or_else(|| codec_error("Ethereum IX undo record length overflowed"))?;
        let expected_length = 4_usize
            .checked_add(transaction_bytes)
            .ok_or_else(|| codec_error("Ethereum IX undo record length overflowed"))?;
        if payload.len() != expected_length {
            return Err(codec_error("Ethereum IX undo record has an invalid length"));
        }

        let mut affected_transactions = Vec::with_capacity(count);
        for encoded_transaction in payload[4..].chunks_exact(32) {
            let mut transaction_id = [0_u8; 32];
            transaction_id.copy_from_slice(encoded_transaction);
            affected_transactions.push(EthereumTransactionId(transaction_id));
        }

        Ok(EthereumUndo {
            affected_transactions,
        })
    }
}

fn write_header(encoded: &mut Vec<u8>, record_kind: u8) {
    encoded.extend_from_slice(MAGIC);
    encoded.push(VERSION_V1);
    encoded.push(record_kind);
}

fn read_payload(encoded: &[u8], expected_record_kind: u8) -> Result<&[u8], IndexError> {
    if encoded.len() < HEADER_LENGTH {
        return Err(codec_error("Ethereum IX record is shorter than its header"));
    }
    if encoded.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
        return Err(codec_error(
            "Ethereum IX record has an invalid magic prefix",
        ));
    }
    if encoded[MAGIC.len()] != VERSION_V1 {
        return Err(codec_error("Ethereum IX record version is not supported"));
    }
    if encoded[MAGIC.len() + 1] != expected_record_kind {
        return Err(codec_error("Ethereum IX record has the wrong record kind"));
    }
    Ok(&encoded[HEADER_LENGTH..])
}

fn codec_error(message: &'static str) -> IndexError {
    IndexError::new(IndexErrorKind::Storage, message, false)
}

#[cfg(test)]
mod tests {
    use indexing::{IndexErrorKind, IndexRecordCodec};

    use super::*;

    #[test]
    fn address_target_round_trips_through_v1_format() {
        let codec = EthereumIndexRecordCodec;
        let target = EthereumWatchTarget::Address(EthereumAddress([0x11; 20]));

        let encoded = codec
            .encode_target(&target)
            .expect("a valid address target must encode");

        assert_eq!(&encoded[..4], b"ETHI");
        assert_eq!(encoded[4], VERSION_V1);
        assert_eq!(
            codec
                .decode_target(&encoded)
                .expect("the encoded address target must decode"),
            target
        );
    }

    #[test]
    fn transaction_target_round_trips_through_v1_format() {
        let codec = EthereumIndexRecordCodec;
        let target = EthereumWatchTarget::Transaction(EthereumTransactionId([0x22; 32]));

        let encoded = codec
            .encode_target(&target)
            .expect("a valid transaction target must encode");

        assert_eq!(
            codec
                .decode_target(&encoded)
                .expect("the encoded transaction target must decode"),
            target
        );
    }

    #[test]
    fn undo_round_trips_without_serializing_the_rust_layout() {
        let codec = EthereumIndexRecordCodec;
        let undo = EthereumUndo {
            affected_transactions: vec![
                EthereumTransactionId([0x33; 32]),
                EthereumTransactionId([0x44; 32]),
            ],
        };

        let encoded = codec
            .encode_undo(&undo)
            .expect("valid undo data must encode");

        assert_eq!(&encoded[..4], b"ETHI");
        assert_eq!(encoded[4], VERSION_V1);
        assert_eq!(
            codec
                .decode_undo(&encoded)
                .expect("the encoded undo data must decode"),
            undo
        );
    }

    #[test]
    fn unknown_version_is_rejected_for_target_and_undo_records() {
        let codec = EthereumIndexRecordCodec;
        let mut target = codec
            .encode_target(&EthereumWatchTarget::Address(EthereumAddress([0x55; 20])))
            .expect("a valid target must encode");
        let mut undo = codec
            .encode_undo(&EthereumUndo::default())
            .expect("valid undo data must encode");
        target[4] = VERSION_V1 + 1;
        undo[4] = VERSION_V1 + 1;

        let target_error = codec
            .decode_target(&target)
            .expect_err("an unknown target version must fail");
        let undo_error = codec
            .decode_undo(&undo)
            .expect_err("an unknown undo version must fail");

        assert_eq!(target_error.kind, IndexErrorKind::Storage);
        assert!(!target_error.retryable);
        assert_eq!(undo_error.kind, IndexErrorKind::Storage);
        assert!(!undo_error.retryable);
    }

    #[test]
    fn wrong_record_kind_and_trailing_bytes_are_rejected() {
        let codec = EthereumIndexRecordCodec;
        let encoded_undo = codec
            .encode_undo(&EthereumUndo::default())
            .expect("valid undo data must encode");
        let mut encoded_target = codec
            .encode_target(&EthereumWatchTarget::Transaction(EthereumTransactionId(
                [0x66; 32],
            )))
            .expect("a valid target must encode");
        encoded_target.push(0);

        assert!(codec.decode_target(&encoded_undo).is_err());
        assert!(codec.decode_target(&encoded_target).is_err());
    }
}
