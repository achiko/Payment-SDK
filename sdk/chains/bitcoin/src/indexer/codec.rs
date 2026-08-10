use indexing::{
    BlockHeight, IndexError, IndexErrorKind, IndexRecordCodec, ProjectionBatch, ProjectionMutation,
};

use crate::{BitcoinAddress, BitcoinTransactionId, Satoshi};

use super::{
    BitcoinIndexedOutput, BitcoinOutPoint, BitcoinProjectionKey, BitcoinUndo, BitcoinUtxoKey,
    BitcoinUtxoProjection, BitcoinWatchTarget,
};

const RECORD_MAGIC: &[u8; 4] = b"BTIX";
const PROJECTION_KEY_MAGIC: &[u8; 4] = b"BTUO";
const UTXO_VALUE_MAGIC: &[u8; 4] = b"BTUV";
const SPENT_VALUE_MAGIC: &[u8; 4] = b"BTUS";
const VERSION_V1: u8 = 1;
const HEADER_LENGTH: usize = 6;
const PROJECTION_KEY_MINIMUM_LENGTH: usize = 4 + 1 + 1 + 2 + 1 + 32 + 4;

const TARGET_RECORD: u8 = 1;
const UNDO_RECORD: u8 = 2;
const ADDRESS_TARGET: u8 = 1;
const TRANSACTION_TARGET: u8 = 2;
const UTXO_CREATION_KEY: u8 = 1;
const UTXO_SPENT_MARKER_KEY: u8 = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct BitcoinIndexRecordCodec;

impl BitcoinIndexRecordCodec {
    /// Prefix for ordered scans of creation records for one canonical address.
    pub fn utxo_key_prefix(address: &BitcoinAddress) -> Result<Vec<u8>, IndexError> {
        projection_key_prefix(address, UTXO_CREATION_KEY)
    }

    /// Prefix for spent markers for one canonical address.
    pub fn spent_marker_key_prefix(address: &BitcoinAddress) -> Result<Vec<u8>, IndexError> {
        projection_key_prefix(address, UTXO_SPENT_MARKER_KEY)
    }

    pub fn utxo_key(
        address: &BitcoinAddress,
        outpoint: BitcoinOutPoint,
    ) -> Result<Vec<u8>, IndexError> {
        projection_key(address, outpoint, UTXO_CREATION_KEY)
    }

    pub fn spent_marker_key(
        address: &BitcoinAddress,
        outpoint: BitcoinOutPoint,
    ) -> Result<Vec<u8>, IndexError> {
        projection_key(address, outpoint, UTXO_SPENT_MARKER_KEY)
    }

    pub fn decode_projection_key(key: &[u8]) -> Result<BitcoinProjectionKey, IndexError> {
        let (kind, key) = decode_projection_key(key)?;
        match kind {
            UTXO_CREATION_KEY => Ok(BitcoinProjectionKey::Utxo {
                address: key.address,
                outpoint: key.outpoint,
            }),
            UTXO_SPENT_MARKER_KEY => Ok(BitcoinProjectionKey::SpentMarker {
                address: key.address,
                outpoint: key.outpoint,
            }),
            _ => Err(codec_error("Bitcoin projection key kind is unsupported")),
        }
    }

    pub fn decode_utxo_entry(key: &[u8], value: &[u8]) -> Result<BitcoinIndexedOutput, IndexError> {
        let (kind, key) = decode_projection_key(key)?;
        if kind != UTXO_CREATION_KEY {
            return Err(codec_error(
                "Bitcoin UTXO value uses a non-creation projection key",
            ));
        }
        decode_utxo_value(&key, value)
    }

    pub fn decode_spent_marker_entry(
        key: &[u8],
        value: &[u8],
    ) -> Result<BitcoinUtxoKey, IndexError> {
        let (kind, key) = decode_projection_key(key)?;
        if kind != UTXO_SPENT_MARKER_KEY
            || value.len() != 5
            || value.get(..4) != Some(SPENT_VALUE_MAGIC.as_slice())
            || value[4] != VERSION_V1
        {
            return Err(codec_error(
                "Bitcoin spent-marker entry has an invalid key or value",
            ));
        }
        Ok(key)
    }

    pub fn projection_batch(
        projection: &BitcoinUtxoProjection,
    ) -> Result<ProjectionBatch, IndexError> {
        validate_projection(projection)?;
        let mut mutations = Vec::with_capacity(
            projection
                .creates
                .len()
                .checked_add(projection.spends.len())
                .and_then(|length| length.checked_add(projection.conditional_spends.len()))
                .ok_or_else(|| codec_error("Bitcoin UTXO projection length overflowed"))?,
        );
        for output in &projection.creates {
            mutations.push(ProjectionMutation::Put {
                key: Self::utxo_key(&output.address, output.outpoint)?,
                value: encode_utxo_value(output)?,
            });
        }
        for output in &projection.spends {
            mutations.push(ProjectionMutation::Put {
                key: Self::spent_marker_key(&output.address, output.outpoint)?,
                value: spent_marker_value(),
            });
        }
        for output in &projection.conditional_spends {
            mutations.push(ProjectionMutation::PutIfPresent {
                required_key: Self::utxo_key(&output.address, output.outpoint)?,
                key: Self::spent_marker_key(&output.address, output.outpoint)?,
                value: spent_marker_value(),
            });
        }
        Ok(ProjectionBatch::new(mutations))
    }
}

impl IndexRecordCodec for BitcoinIndexRecordCodec {
    type Target = BitcoinWatchTarget;
    type Undo = BitcoinUndo;

    fn encode_target(&self, target: &Self::Target) -> Result<Vec<u8>, IndexError> {
        let mut encoded = Vec::new();
        write_header(&mut encoded, TARGET_RECORD);
        match target {
            BitcoinWatchTarget::Address(address) => {
                encoded.push(ADDRESS_TARGET);
                write_bytes_u32(&mut encoded, address.0.as_bytes(), "Bitcoin address target")?;
            }
            BitcoinWatchTarget::Transaction(transaction_id) => {
                encoded.push(TRANSACTION_TARGET);
                encoded.extend_from_slice(&transaction_id.0);
            }
        }
        Ok(encoded)
    }

    fn decode_target(&self, encoded: &[u8]) -> Result<Self::Target, IndexError> {
        let payload = read_payload(encoded, TARGET_RECORD)?;
        let mut reader = Reader::new(payload);
        let target = match reader.byte("Bitcoin IX target kind")? {
            ADDRESS_TARGET => {
                let address = reader.bytes_u32("Bitcoin IX address target")?;
                let address = std::str::from_utf8(address)
                    .map_err(|_| codec_error("Bitcoin IX address target is not UTF-8"))?;
                BitcoinWatchTarget::Address(BitcoinAddress(address.to_owned()))
            }
            TRANSACTION_TARGET => BitcoinWatchTarget::Transaction(BitcoinTransactionId(
                reader.array::<32>("Bitcoin IX transaction target")?,
            )),
            _ => return Err(codec_error("Bitcoin IX target kind is not supported")),
        };
        reader.finish("Bitcoin IX target record")?;
        Ok(target)
    }

    fn encode_undo(&self, undo: &Self::Undo) -> Result<Vec<u8>, IndexError> {
        validate_undo(undo)?;
        let mut encoded = Vec::new();
        write_header(&mut encoded, UNDO_RECORD);
        write_key_set(
            &mut encoded,
            &undo.remove_created,
            "Bitcoin undo creation keys",
        )?;
        write_key_set(
            &mut encoded,
            &undo.remove_spent_markers,
            "Bitcoin undo spent-marker keys",
        )?;
        Ok(encoded)
    }

    fn decode_undo(&self, encoded: &[u8]) -> Result<Self::Undo, IndexError> {
        let payload = read_payload(encoded, UNDO_RECORD)?;
        let mut reader = Reader::new(payload);
        let remove_created = reader.key_set("Bitcoin undo creation keys")?;
        let remove_spent_markers = reader.key_set("Bitcoin undo spent-marker keys")?;
        reader.finish("Bitcoin IX undo record")?;
        let undo = BitcoinUndo {
            remove_created,
            remove_spent_markers,
        };
        validate_undo(&undo)?;
        Ok(undo)
    }

    fn rollback_projection(&self, undo: &Self::Undo) -> Result<ProjectionBatch, IndexError> {
        validate_undo(undo)?;
        let mut mutations = Vec::with_capacity(
            undo.remove_spent_markers
                .len()
                .checked_add(undo.remove_created.len())
                .ok_or_else(|| codec_error("Bitcoin rollback projection length overflowed"))?,
        );
        for key in &undo.remove_spent_markers {
            mutations.push(ProjectionMutation::Delete {
                key: Self::spent_marker_key(&key.address, key.outpoint)?,
            });
        }
        for key in &undo.remove_created {
            mutations.push(ProjectionMutation::Delete {
                key: Self::utxo_key(&key.address, key.outpoint)?,
            });
        }
        Ok(ProjectionBatch::new(mutations))
    }
}

fn projection_key_prefix(address: &BitcoinAddress, kind: u8) -> Result<Vec<u8>, IndexError> {
    let address_length = u16::try_from(address.0.len())
        .map_err(|_| codec_error("Bitcoin projection address is too long"))?;
    let mut prefix = Vec::with_capacity(8 + address.0.len());
    prefix.extend_from_slice(PROJECTION_KEY_MAGIC);
    prefix.push(VERSION_V1);
    prefix.push(kind);
    prefix.extend_from_slice(&address_length.to_be_bytes());
    prefix.extend_from_slice(address.0.as_bytes());
    Ok(prefix)
}

fn projection_key(
    address: &BitcoinAddress,
    outpoint: BitcoinOutPoint,
    kind: u8,
) -> Result<Vec<u8>, IndexError> {
    let mut key = projection_key_prefix(address, kind)?;
    key.extend_from_slice(&outpoint.transaction_id.0);
    key.extend_from_slice(&outpoint.output_index.to_be_bytes());
    Ok(key)
}

fn decode_projection_key(encoded: &[u8]) -> Result<(u8, BitcoinUtxoKey), IndexError> {
    if encoded.len() < PROJECTION_KEY_MINIMUM_LENGTH
        || encoded.get(..4) != Some(PROJECTION_KEY_MAGIC.as_slice())
        || encoded[4] != VERSION_V1
    {
        return Err(codec_error(
            "Bitcoin projection key has an invalid length, magic, or version",
        ));
    }
    let kind = encoded[5];
    let mut length = [0_u8; 2];
    length.copy_from_slice(&encoded[6..8]);
    let address_length = usize::from(u16::from_be_bytes(length));
    let outpoint_start = 8_usize
        .checked_add(address_length)
        .ok_or_else(|| codec_error("Bitcoin projection address length overflowed"))?;
    let expected_length = outpoint_start
        .checked_add(36)
        .ok_or_else(|| codec_error("Bitcoin projection key length overflowed"))?;
    if encoded.len() != expected_length {
        return Err(codec_error("Bitcoin projection key has an invalid length"));
    }
    let address = std::str::from_utf8(&encoded[8..outpoint_start])
        .map_err(|_| codec_error("Bitcoin projection key address is not UTF-8"))?;
    let mut transaction_id = [0_u8; 32];
    transaction_id.copy_from_slice(&encoded[outpoint_start..outpoint_start + 32]);
    let mut output_index = [0_u8; 4];
    output_index.copy_from_slice(&encoded[outpoint_start + 32..expected_length]);
    Ok((
        kind,
        BitcoinUtxoKey {
            address: BitcoinAddress(address.to_owned()),
            outpoint: BitcoinOutPoint {
                transaction_id: BitcoinTransactionId(transaction_id),
                output_index: u32::from_be_bytes(output_index),
            },
        },
    ))
}

fn encode_utxo_value(output: &BitcoinIndexedOutput) -> Result<Vec<u8>, IndexError> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(UTXO_VALUE_MAGIC);
    encoded.push(VERSION_V1);
    encoded.extend_from_slice(&output.value.0.to_be_bytes());
    encoded.extend_from_slice(&output.created_height.0.to_be_bytes());
    encoded.push(u8::from(output.coinbase));
    write_bytes_u32(
        &mut encoded,
        &output.script_pubkey,
        "Bitcoin UTXO scriptPubKey",
    )?;
    write_bytes_u32(
        &mut encoded,
        output.address.0.as_bytes(),
        "Bitcoin UTXO address",
    )?;
    Ok(encoded)
}

fn decode_utxo_value(
    key: &BitcoinUtxoKey,
    encoded: &[u8],
) -> Result<BitcoinIndexedOutput, IndexError> {
    if encoded.len() < 5
        || encoded.get(..4) != Some(UTXO_VALUE_MAGIC.as_slice())
        || encoded[4] != VERSION_V1
    {
        return Err(codec_error(
            "Bitcoin UTXO value has an invalid magic or version",
        ));
    }
    let mut reader = Reader::new(&encoded[5..]);
    let value = Satoshi(u64::from_be_bytes(reader.array::<8>("Bitcoin UTXO value")?));
    let created_height = BlockHeight(u64::from_be_bytes(
        reader.array::<8>("Bitcoin UTXO creation height")?,
    ));
    let coinbase = match reader.byte("Bitcoin UTXO coinbase flag")? {
        0 => false,
        1 => true,
        _ => return Err(codec_error("Bitcoin UTXO coinbase flag is invalid")),
    };
    let script_pubkey = reader.bytes_u32("Bitcoin UTXO scriptPubKey")?.to_vec();
    let address = reader.bytes_u32("Bitcoin UTXO address")?;
    let address = std::str::from_utf8(address)
        .map_err(|_| codec_error("Bitcoin UTXO address is not UTF-8"))?;
    reader.finish("Bitcoin UTXO value")?;
    if address != key.address.0 {
        return Err(codec_error(
            "Bitcoin UTXO key address does not match its value",
        ));
    }
    Ok(BitcoinIndexedOutput {
        outpoint: key.outpoint,
        value,
        script_pubkey,
        address: key.address.clone(),
        created_height,
        coinbase,
    })
}

fn spent_marker_value() -> Vec<u8> {
    let mut value = Vec::with_capacity(5);
    value.extend_from_slice(SPENT_VALUE_MAGIC);
    value.push(VERSION_V1);
    value
}

fn write_header(encoded: &mut Vec<u8>, record_kind: u8) {
    encoded.extend_from_slice(RECORD_MAGIC);
    encoded.push(VERSION_V1);
    encoded.push(record_kind);
}

fn read_payload(encoded: &[u8], expected_kind: u8) -> Result<&[u8], IndexError> {
    if encoded.len() < HEADER_LENGTH
        || encoded.get(..4) != Some(RECORD_MAGIC.as_slice())
        || encoded[4] != VERSION_V1
        || encoded[5] != expected_kind
    {
        return Err(codec_error(
            "Bitcoin IX record has an invalid header or record kind",
        ));
    }
    Ok(&encoded[HEADER_LENGTH..])
}

fn write_key_set(
    encoded: &mut Vec<u8>,
    keys: &[BitcoinUtxoKey],
    context: &'static str,
) -> Result<(), IndexError> {
    let count = u32::try_from(keys.len())
        .map_err(|_| codec_error(format!("{context} contains too many items")))?;
    encoded.extend_from_slice(&count.to_be_bytes());
    for key in keys {
        write_bytes_u32(encoded, key.address.0.as_bytes(), context)?;
        encoded.extend_from_slice(&key.outpoint.transaction_id.0);
        encoded.extend_from_slice(&key.outpoint.output_index.to_be_bytes());
    }
    Ok(())
}

fn ensure_unique_keys(keys: &[BitcoinUtxoKey], context: &'static str) -> Result<(), IndexError> {
    let unique: std::collections::BTreeSet<_> = keys.iter().collect();
    if unique.len() == keys.len() {
        Ok(())
    } else {
        Err(codec_error(format!("{context} contains duplicates")))
    }
}

fn validate_undo(undo: &BitcoinUndo) -> Result<(), IndexError> {
    ensure_unique_keys(&undo.remove_created, "Bitcoin undo creation keys")?;
    ensure_unique_keys(&undo.remove_spent_markers, "Bitcoin undo spent-marker keys")?;
    let creations: std::collections::BTreeSet<_> = undo.remove_created.iter().collect();
    if undo
        .remove_spent_markers
        .iter()
        .any(|key| creations.contains(key))
    {
        return Err(codec_error(
            "Bitcoin undo contains the same key as a creation and spent marker",
        ));
    }
    Ok(())
}

fn validate_projection(projection: &BitcoinUtxoProjection) -> Result<(), IndexError> {
    let mut creations = std::collections::BTreeMap::new();
    for output in &projection.creates {
        if creations.insert(output.outpoint, &output.address).is_some() {
            return Err(codec_error(
                "Bitcoin projection contains a duplicate created outpoint",
            ));
        }
    }
    let mut spends = std::collections::BTreeMap::new();
    for output in &projection.spends {
        if spends.insert(output.outpoint, &output.address).is_some() {
            return Err(codec_error(
                "Bitcoin projection contains a duplicate spent outpoint",
            ));
        }
        if creations.contains_key(&output.outpoint) {
            return Err(codec_error(
                "Bitcoin projection contains a non-netted same-block outpoint",
            ));
        }
    }
    for output in &projection.conditional_spends {
        if spends.insert(output.outpoint, &output.address).is_some() {
            return Err(codec_error(
                "Bitcoin projection contains a duplicate spent outpoint",
            ));
        }
        if creations.contains_key(&output.outpoint) {
            return Err(codec_error(
                "Bitcoin projection contains a non-netted same-block outpoint",
            ));
        }
    }
    Ok(())
}

fn write_bytes_u32(
    encoded: &mut Vec<u8>,
    bytes: &[u8],
    context: &'static str,
) -> Result<(), IndexError> {
    let length =
        u32::try_from(bytes.len()).map_err(|_| codec_error(format!("{context} is too long")))?;
    encoded.extend_from_slice(&length.to_be_bytes());
    encoded.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn byte(&mut self, context: &'static str) -> Result<u8, IndexError> {
        self.array::<1>(context).map(|value| value[0])
    }

    fn array<const N: usize>(&mut self, context: &'static str) -> Result<[u8; N], IndexError> {
        let end = self
            .position
            .checked_add(N)
            .ok_or_else(|| codec_error(format!("{context} length overflowed")))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| codec_error(format!("{context} is truncated")))?;
        self.position = end;
        let mut result = [0_u8; N];
        result.copy_from_slice(bytes);
        Ok(result)
    }

    fn count(&mut self, context: &'static str) -> Result<usize, IndexError> {
        usize::try_from(u32::from_be_bytes(self.array::<4>(context)?))
            .map_err(|_| codec_error(format!("{context} is unsupported on this platform")))
    }

    fn bytes_u32(&mut self, context: &'static str) -> Result<&'a [u8], IndexError> {
        let length = self.count(context)?;
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| codec_error(format!("{context} length overflowed")))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| codec_error(format!("{context} is truncated")))?;
        self.position = end;
        Ok(bytes)
    }

    fn key_set(&mut self, context: &'static str) -> Result<Vec<BitcoinUtxoKey>, IndexError> {
        let count = self.count(context)?;
        let mut keys = Vec::with_capacity(count);
        for _ in 0..count {
            let address = self.bytes_u32(context)?;
            let address = std::str::from_utf8(address)
                .map_err(|_| codec_error(format!("{context} address is not UTF-8")))?;
            let transaction_id = BitcoinTransactionId(self.array::<32>(context)?);
            let output_index = u32::from_be_bytes(self.array::<4>(context)?);
            keys.push(BitcoinUtxoKey {
                address: BitcoinAddress(address.to_owned()),
                outpoint: BitcoinOutPoint {
                    transaction_id,
                    output_index,
                },
            });
        }
        Ok(keys)
    }

    fn finish(&self, context: &'static str) -> Result<(), IndexError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(codec_error(format!("{context} has trailing bytes")))
        }
    }
}

fn codec_error(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Storage, message, false)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn output(byte: u8, output_index: u32) -> BitcoinIndexedOutput {
        BitcoinIndexedOutput {
            outpoint: BitcoinOutPoint {
                transaction_id: BitcoinTransactionId([byte; 32]),
                output_index,
            },
            value: Satoshi(42_000),
            script_pubkey: vec![0x00, 0x14, byte],
            address: BitcoinAddress(format!("bcrt1q{byte}")),
            created_height: BlockHeight(100),
            coinbase: false,
        }
    }

    fn key(output: &BitcoinIndexedOutput) -> BitcoinUtxoKey {
        BitcoinUtxoKey {
            address: output.address.clone(),
            outpoint: output.outpoint,
        }
    }

    #[test]
    fn targets_and_undo_round_trip_through_versioned_format() {
        let codec = BitcoinIndexRecordCodec;
        for target in [
            BitcoinWatchTarget::Address(BitcoinAddress("bcrt1qexample".to_owned())),
            BitcoinWatchTarget::Transaction(BitcoinTransactionId([2; 32])),
        ] {
            let encoded = codec
                .encode_target(&target)
                .expect("valid target must encode");
            assert_eq!(&encoded[..4], RECORD_MAGIC);
            assert_eq!(
                codec
                    .decode_target(&encoded)
                    .expect("encoded target must decode"),
                target
            );
        }

        let first = output(3, 1);
        let second = output(4, 2);
        let undo = BitcoinUndo {
            remove_created: vec![key(&first)],
            remove_spent_markers: vec![key(&second)],
        };
        let encoded = codec.encode_undo(&undo).expect("valid undo must encode");
        assert_eq!(
            codec
                .decode_undo(&encoded)
                .expect("encoded undo must decode"),
            undo
        );
    }

    #[test]
    fn creation_and_spent_marker_are_disjoint_and_order_independent() {
        let output = output(5, 7);
        let creation = BitcoinIndexRecordCodec::projection_batch(&BitcoinUtxoProjection {
            creates: vec![output.clone()],
            spends: Vec::new(),
            conditional_spends: Vec::new(),
        })
        .expect("creation projection must encode");
        let spending = BitcoinIndexRecordCodec::projection_batch(&BitcoinUtxoProjection {
            creates: Vec::new(),
            spends: vec![key(&output)],
            conditional_spends: Vec::new(),
        })
        .expect("spend projection must encode");

        let apply = |batches: [&ProjectionBatch; 2]| {
            let mut state = BTreeMap::new();
            for batch in batches {
                for mutation in &batch.mutations {
                    match mutation {
                        ProjectionMutation::Put { key, value } => {
                            state.insert(key.clone(), value.clone());
                        }
                        ProjectionMutation::PutIfPresent { .. } => {
                            panic!("unconditional test batches must not contain conditional puts")
                        }
                        ProjectionMutation::Delete { key } => {
                            state.remove(key);
                        }
                    }
                }
            }
            state
        };
        assert_eq!(apply([&creation, &spending]), apply([&spending, &creation]));

        let ProjectionMutation::Put {
            key: creation_key,
            value: creation_value,
        } = &creation.mutations[0]
        else {
            panic!("creation must be a put");
        };
        let ProjectionMutation::Put {
            key: marker_key,
            value: marker_value,
        } = &spending.mutations[0]
        else {
            panic!("spend must be a marker put");
        };
        assert_ne!(creation_key, marker_key);
        assert_eq!(
            BitcoinIndexRecordCodec::decode_utxo_entry(creation_key, creation_value)
                .expect("creation entry must decode"),
            output
        );
        assert_eq!(
            BitcoinIndexRecordCodec::decode_spent_marker_entry(marker_key, marker_value)
                .expect("marker entry must decode"),
            key(&output)
        );
    }

    #[test]
    fn rollback_deletes_marker_and_creation_keys() {
        let created = output(6, 0);
        let spent = output(7, 0);
        let undo = BitcoinUndo {
            remove_created: vec![key(&created)],
            remove_spent_markers: vec![key(&spent)],
        };

        let rollback = BitcoinIndexRecordCodec
            .rollback_projection(&undo)
            .expect("valid rollback must encode");

        assert_eq!(rollback.mutations.len(), 2);
        assert!(
            rollback
                .mutations
                .iter()
                .all(|mutation| matches!(mutation, ProjectionMutation::Delete { .. }))
        );
        assert_ne!(rollback.mutations[0].key(), rollback.mutations[1].key());
    }

    #[test]
    fn unknown_versions_duplicates_and_trailing_bytes_fail_closed() {
        let codec = BitcoinIndexRecordCodec;
        let mut target = codec
            .encode_target(&BitcoinWatchTarget::Transaction(BitcoinTransactionId(
                [8; 32],
            )))
            .expect("valid target must encode");
        target[4] = VERSION_V1 + 1;
        assert!(codec.decode_target(&target).is_err());

        let output = output(9, 1);
        let duplicate = key(&output);
        let error = codec
            .encode_undo(&BitcoinUndo {
                remove_created: vec![duplicate.clone(), duplicate],
                remove_spent_markers: Vec::new(),
            })
            .expect_err("duplicate undo must fail before persistence");
        assert_eq!(error.kind, IndexErrorKind::Storage);

        let mut undo = codec
            .encode_undo(&BitcoinUndo::default())
            .expect("valid undo must encode");
        undo.push(0);
        assert!(codec.decode_undo(&undo).is_err());
    }
}
