use std::mem::size_of;

use bincode::{Decode, Encode};
use storage::{Key, Namespace, StorageError, StorageErrorKind, StoredValue, Value, Version};

const KEY_FORMAT_VERSION: u8 = 1;
const VALUE_MAGIC: &[u8; 4] = b"W3KV";
const GLOBAL_VERSION_MAGIC: &[u8; 4] = b"W3GV";
const FRAME_VERSION: u8 = 1;
const FRAME_PREFIX_LEN: usize = 5;
const RECORD_V1_PREFIX_LEN: usize = 16;
const GLOBAL_VERSION_V1_LEN: usize = 8;
const MAX_STORED_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Decode, Encode, PartialEq, Eq)]
struct RecordV1 {
    storage_version: u64,
    payload: Vec<u8>,
}

#[derive(Debug, Decode, Encode, PartialEq, Eq)]
struct GlobalVersionV1 {
    version: u64,
}

pub(crate) fn namespace_prefix(namespace: &Namespace) -> Result<Vec<u8>, StorageError> {
    let namespace_bytes = namespace.0.as_bytes();
    let namespace_len = u32::try_from(namespace_bytes.len())
        .map_err(|_| invalid_request("namespace length exceeds the storage key format limit"))?;
    let capacity = 1usize
        .checked_add(size_of::<u32>())
        .and_then(|length| length.checked_add(namespace_bytes.len()))
        .ok_or_else(|| invalid_request("namespace length overflows the storage key format"))?;

    let mut encoded = Vec::with_capacity(capacity);
    encoded.push(KEY_FORMAT_VERSION);
    encoded.extend_from_slice(&namespace_len.to_be_bytes());
    encoded.extend_from_slice(namespace_bytes);
    Ok(encoded)
}

pub(crate) fn encode_physical_key(
    namespace: &Namespace,
    key: &Key,
) -> Result<Vec<u8>, StorageError> {
    let mut encoded = namespace_prefix(namespace)?;
    encoded
        .len()
        .checked_add(key.0.len())
        .ok_or_else(|| invalid_request("logical key length overflows the storage key format"))?;
    encoded.extend_from_slice(&key.0);
    Ok(encoded)
}

pub(crate) fn decode_physical_key(
    physical: &[u8],
    expected_namespace: &Namespace,
) -> Result<Key, StorageError> {
    if physical.len() < 1 + size_of::<u32>() {
        return Err(corrupt_data("physical key is shorter than its header"));
    }
    if physical[0] != KEY_FORMAT_VERSION {
        return Err(corrupt_data(
            "physical key has an unsupported format version",
        ));
    }

    let namespace_len = read_u32(&physical[1..5])? as usize;
    let key_offset = 5usize
        .checked_add(namespace_len)
        .ok_or_else(|| corrupt_data("physical key namespace length overflows"))?;
    if physical.len() < key_offset {
        return Err(corrupt_data(
            "physical key namespace length exceeds the encoded key",
        ));
    }
    if &physical[5..key_offset] != expected_namespace.0.as_bytes() {
        return Err(corrupt_data(
            "physical key does not belong to the requested namespace",
        ));
    }

    Ok(Key(physical[key_offset..].to_vec()))
}

pub(crate) fn encode_stored_value(
    value: &Value,
    version: Version,
) -> Result<Vec<u8>, StorageError> {
    if version.0 == 0 {
        return Err(invalid_request(
            "storage version zero is reserved for an uninitialized database",
        ));
    }
    if value.0.len() > MAX_STORED_PAYLOAD_BYTES {
        return Err(invalid_request(
            "storage value exceeds the physical record size limit",
        ));
    }

    let record = RecordV1 {
        storage_version: version.0,
        payload: value.0.clone(),
    };
    let body = bincode::encode_to_vec(record, record_config())
        .map_err(|error| other(format!("failed to encode the storage value frame: {error}")))?;

    let mut frame = Vec::with_capacity(FRAME_PREFIX_LEN + body.len());
    frame.extend_from_slice(VALUE_MAGIC);
    frame.push(FRAME_VERSION);
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub(crate) fn decode_stored_value(frame: &[u8]) -> Result<StoredValue, StorageError> {
    let body = validate_frame_prefix(frame, VALUE_MAGIC, "storage value")?;
    validate_record_v1_length(body)?;

    let (record, bytes_read) = bincode::decode_from_slice::<RecordV1, _>(body, record_config())
        .map_err(|error| {
            corrupt_data(format!("failed to decode storage value RecordV1: {error}"))
        })?;
    if bytes_read != body.len() {
        return Err(corrupt_data(
            "storage value RecordV1 contains trailing bytes",
        ));
    }
    if record.storage_version == 0 {
        return Err(corrupt_data(
            "storage value RecordV1 contains reserved version zero",
        ));
    }

    Ok(StoredValue {
        value: Value(record.payload),
        version: Version(record.storage_version),
    })
}

pub(crate) fn encode_global_version(version: Version) -> Result<Vec<u8>, StorageError> {
    if version.0 == 0 {
        return Err(invalid_request(
            "persisted global version zero is not a valid commit version",
        ));
    }

    let body = bincode::encode_to_vec(GlobalVersionV1 { version: version.0 }, record_config())
        .map_err(|error| {
            other(format!(
                "failed to encode the global version frame: {error}"
            ))
        })?;
    let mut frame = Vec::with_capacity(FRAME_PREFIX_LEN + body.len());
    frame.extend_from_slice(GLOBAL_VERSION_MAGIC);
    frame.push(FRAME_VERSION);
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub(crate) fn decode_global_version(frame: &[u8]) -> Result<Version, StorageError> {
    let body = validate_frame_prefix(frame, GLOBAL_VERSION_MAGIC, "global version")?;
    if body.len() != GLOBAL_VERSION_V1_LEN {
        return Err(corrupt_data(format!(
            "global version RecordV1 has length {}, expected {GLOBAL_VERSION_V1_LEN}",
            body.len()
        )));
    }

    let (record, bytes_read) =
        bincode::decode_from_slice::<GlobalVersionV1, _>(body, record_config()).map_err(
            |error| corrupt_data(format!("failed to decode global version RecordV1: {error}")),
        )?;
    if bytes_read != body.len() {
        return Err(corrupt_data(
            "global version RecordV1 contains trailing bytes",
        ));
    }
    if record.version == 0 {
        return Err(corrupt_data(
            "global version RecordV1 contains reserved version zero",
        ));
    }

    Ok(Version(record.version))
}

fn record_config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_fixed_int_encoding()
        .with_big_endian()
}

fn validate_frame_prefix<'a>(
    frame: &'a [u8],
    expected_magic: &[u8; 4],
    description: &str,
) -> Result<&'a [u8], StorageError> {
    if frame.len() < FRAME_PREFIX_LEN {
        return Err(corrupt_data(format!(
            "{description} frame is shorter than its header"
        )));
    }
    if &frame[..4] != expected_magic {
        return Err(corrupt_data(format!(
            "{description} frame has invalid magic bytes"
        )));
    }
    if frame[4] != FRAME_VERSION {
        return Err(corrupt_data(format!(
            "{description} frame has unsupported version {}",
            frame[4]
        )));
    }

    Ok(&frame[FRAME_PREFIX_LEN..])
}

fn validate_record_v1_length(body: &[u8]) -> Result<(), StorageError> {
    if body.len() < RECORD_V1_PREFIX_LEN {
        return Err(corrupt_data(
            "storage value RecordV1 is shorter than its fixed fields",
        ));
    }

    let declared_payload_len = read_u64(&body[8..16])?;
    let declared_payload_len = usize::try_from(declared_payload_len)
        .map_err(|_| corrupt_data("storage value RecordV1 payload length exceeds this platform"))?;
    let actual_payload_len = body.len() - RECORD_V1_PREFIX_LEN;
    validate_payload_length(declared_payload_len, actual_payload_len)?;

    Ok(())
}

fn validate_payload_length(
    declared_payload_len: usize,
    actual_payload_len: usize,
) -> Result<(), StorageError> {
    if declared_payload_len > MAX_STORED_PAYLOAD_BYTES {
        return Err(corrupt_data(
            "storage value RecordV1 exceeds the physical record size limit",
        ));
    }
    if declared_payload_len != actual_payload_len {
        return Err(corrupt_data(format!(
            "storage value RecordV1 payload length is {declared_payload_len}, actual length is {actual_payload_len}"
        )));
    }

    Ok(())
}

fn read_u32(bytes: &[u8]) -> Result<u32, StorageError> {
    if bytes.len() != size_of::<u32>() {
        return Err(corrupt_data("invalid encoded u32 length"));
    }
    let mut value = [0_u8; size_of::<u32>()];
    value.copy_from_slice(bytes);
    Ok(u32::from_be_bytes(value))
}

fn read_u64(bytes: &[u8]) -> Result<u64, StorageError> {
    if bytes.len() != size_of::<u64>() {
        return Err(corrupt_data("invalid encoded u64 length"));
    }
    let mut value = [0_u8; size_of::<u64>()];
    value.copy_from_slice(bytes);
    Ok(u64::from_be_bytes(value))
}

fn invalid_request(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn corrupt_data(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::CorruptData,
        message: message.into(),
    }
}

fn other(message: impl Into<String>) -> StorageError {
    StorageError {
        kind: StorageErrorKind::Other,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_key_round_trips() -> Result<(), StorageError> {
        let namespace = Namespace("observations/ethereum-mainnet".to_owned());
        let logical = Key(vec![0, 1, 2, 255]);

        let physical = encode_physical_key(&namespace, &logical)?;

        assert_eq!(decode_physical_key(&physical, &namespace)?, logical);
        Ok(())
    }

    #[test]
    fn stored_value_round_trips() -> Result<(), StorageError> {
        let value = Value(vec![0, 1, 2, 3, 255]);

        let encoded = encode_stored_value(&value, Version(42))?;

        assert_eq!(
            decode_stored_value(&encoded)?,
            StoredValue {
                value,
                version: Version(42),
            }
        );
        Ok(())
    }

    #[test]
    fn malformed_value_length_is_rejected() -> Result<(), StorageError> {
        let mut encoded = encode_stored_value(&Value(vec![1, 2, 3]), Version(1))?;
        encoded.push(4);

        let error = decode_stored_value(&encoded)
            .expect_err("a frame with trailing payload bytes must be rejected");

        assert_eq!(error.kind, StorageErrorKind::CorruptData);
        Ok(())
    }

    #[test]
    fn unsupported_value_frame_is_rejected() -> Result<(), StorageError> {
        let mut encoded = encode_stored_value(&Value(vec![1]), Version(1))?;
        encoded[4] = 2;

        let error = decode_stored_value(&encoded)
            .expect_err("an unsupported frame version must be rejected");

        assert_eq!(error.kind, StorageErrorKind::CorruptData);
        Ok(())
    }

    #[test]
    fn declared_payload_above_the_corruption_limit_is_rejected_without_allocation() {
        let error = validate_payload_length(MAX_STORED_PAYLOAD_BYTES + 1, 0)
            .expect_err("oversized declared payload must fail closed");
        assert_eq!(error.kind, StorageErrorKind::CorruptData);
    }

    #[test]
    fn global_version_round_trips() -> Result<(), StorageError> {
        let encoded = encode_global_version(Version(7))?;
        assert_eq!(decode_global_version(&encoded)?, Version(7));
        Ok(())
    }
}
