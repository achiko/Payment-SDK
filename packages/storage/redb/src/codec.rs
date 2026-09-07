use std::mem::size_of;

use bincode::{Decode, Encode};
use storage::{Error, ErrorKind, Key, Namespace, StoredValue, Value, Version};

const VALUE_MAGIC: &[u8; 4] = b"W3KV";
const GLOBAL_VERSION_MAGIC: &[u8; 4] = b"W3GV";
const FRAME_PREFIX_LEN: usize = 4;
const RECORD_PREFIX_LEN: usize = 16;
const GLOBAL_VERSION_LEN: usize = 8;
const MAX_STORED_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Decode, Encode, PartialEq, Eq)]
pub(crate) struct StoredRecord {
    storage_version: u64,
    payload: Vec<u8>,
}

#[derive(Debug, Decode, Encode, PartialEq, Eq)]
pub(crate) struct GlobalVersion {
    version: u64,
}

pub(crate) fn namespace_prefix(namespace: &Namespace) -> Result<Vec<u8>, Error> {
    let namespace_bytes = namespace.0.as_bytes();
    let namespace_len = u32::try_from(namespace_bytes.len())
        .map_err(|_| invalid_request("namespace length exceeds the storage key format limit"))?;
    let capacity = size_of::<u32>()
        .checked_add(namespace_bytes.len())
        .ok_or_else(|| invalid_request("namespace length overflows the storage key format"))?;

    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(&namespace_len.to_be_bytes());
    encoded.extend_from_slice(namespace_bytes);
    Ok(encoded)
}

pub(crate) fn encode_physical_key(namespace: &Namespace, key: &Key) -> Result<Vec<u8>, Error> {
    let mut encoded = namespace_prefix(namespace)?;
    encoded
        .len()
        .checked_add(key.0.len())
        .ok_or_else(|| invalid_request("logical key length overflows the storage key format"))?;
    encoded.extend_from_slice(&key.0);
    Ok(encoded)
}

// design-lint: allow unclassified-free-function -- adapter-owned physical key parsing validates the requested foreign Namespace before producing a backend-neutral Key
pub(crate) fn decode_physical_key(
    physical: &[u8],
    expected_namespace: &Namespace,
) -> Result<Key, Error> {
    if physical.len() < size_of::<u32>() {
        return Err(Error::corrupt_data(
            "physical key is shorter than its header",
        ));
    }

    let namespace_len = read_u32(&physical[..4])? as usize;
    let key_offset = 4usize
        .checked_add(namespace_len)
        .ok_or_else(|| Error::corrupt_data("physical key namespace length overflows"))?;
    if physical.len() < key_offset {
        return Err(Error::corrupt_data(
            "physical key namespace length exceeds the encoded key",
        ));
    }
    if &physical[4..key_offset] != expected_namespace.0.as_bytes() {
        return Err(Error::corrupt_data(
            "physical key does not belong to the requested namespace",
        ));
    }

    Ok(Key(physical[key_offset..].to_vec()))
}

pub(crate) fn encode_stored_value(value: &Value, version: Version) -> Result<Vec<u8>, Error> {
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

    let record = StoredRecord {
        storage_version: version.0,
        payload: value.0.clone(),
    };
    let body = bincode::encode_to_vec(record, record_config())
        .map_err(|error| other(format!("failed to encode the storage value frame: {error}")))?;

    let mut frame = Vec::with_capacity(FRAME_PREFIX_LEN + body.len());
    frame.extend_from_slice(VALUE_MAGIC);
    frame.extend_from_slice(&body);
    Ok(frame)
}

impl TryFrom<&[u8]> for StoredRecord {
    type Error = Error;

    fn try_from(frame: &[u8]) -> Result<Self, Self::Error> {
        let body = validate_frame_prefix(frame, VALUE_MAGIC, "storage value")?;
        validate_record_length(body)?;

        let (record, bytes_read) =
            bincode::decode_from_slice::<StoredRecord, _>(body, record_config()).map_err(
                |error| {
                    Error::corrupt_data(format!("failed to decode storage value record: {error}"))
                },
            )?;
        if bytes_read != body.len() {
            return Err(Error::corrupt_data(
                "storage value record contains trailing bytes",
            ));
        }
        if record.storage_version == 0 {
            return Err(Error::corrupt_data(
                "storage value record has an invalid commit version",
            ));
        }

        Ok(record)
    }
}

impl From<StoredRecord> for StoredValue {
    fn from(record: StoredRecord) -> Self {
        Self {
            value: Value(record.payload),
            version: Version(record.storage_version),
        }
    }
}

pub(crate) fn encode_global_version(version: Version) -> Result<Vec<u8>, Error> {
    if version.0 == 0 {
        return Err(invalid_request(
            "persisted global version zero is not a valid commit version",
        ));
    }

    let body = bincode::encode_to_vec(GlobalVersion { version: version.0 }, record_config())
        .map_err(|error| {
            other(format!(
                "failed to encode the global version frame: {error}"
            ))
        })?;
    let mut frame = Vec::with_capacity(FRAME_PREFIX_LEN + body.len());
    frame.extend_from_slice(GLOBAL_VERSION_MAGIC);
    frame.extend_from_slice(&body);
    Ok(frame)
}

impl TryFrom<&[u8]> for GlobalVersion {
    type Error = Error;

    fn try_from(frame: &[u8]) -> Result<Self, Self::Error> {
        let body = validate_frame_prefix(frame, GLOBAL_VERSION_MAGIC, "global version")?;
        if body.len() != GLOBAL_VERSION_LEN {
            return Err(Error::corrupt_data(format!(
                "global version record has length {}, expected {GLOBAL_VERSION_LEN}",
                body.len()
            )));
        }

        let (record, bytes_read) =
            bincode::decode_from_slice::<GlobalVersion, _>(body, record_config()).map_err(
                |error| {
                    Error::corrupt_data(format!("failed to decode global version record: {error}"))
                },
            )?;
        if bytes_read != body.len() {
            return Err(Error::corrupt_data(
                "global version record contains trailing bytes",
            ));
        }
        if record.version == 0 {
            return Err(Error::corrupt_data(
                "global version record has an invalid commit version",
            ));
        }

        Ok(record)
    }
}

impl From<GlobalVersion> for Version {
    fn from(record: GlobalVersion) -> Self {
        Self(record.version)
    }
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
) -> Result<&'a [u8], Error> {
    if frame.len() < FRAME_PREFIX_LEN {
        return Err(Error::corrupt_data(format!(
            "{description} frame is shorter than its header"
        )));
    }
    if &frame[..4] != expected_magic {
        return Err(Error::corrupt_data(format!(
            "{description} frame has invalid magic bytes"
        )));
    }
    Ok(&frame[FRAME_PREFIX_LEN..])
}

fn validate_record_length(body: &[u8]) -> Result<(), Error> {
    if body.len() < RECORD_PREFIX_LEN {
        return Err(Error::corrupt_data(
            "storage value record is shorter than its fixed fields",
        ));
    }

    let declared_payload_len = read_u64(&body[8..16])?;
    let declared_payload_len = usize::try_from(declared_payload_len).map_err(|_| {
        Error::corrupt_data("storage value record payload length exceeds this platform")
    })?;
    let actual_payload_len = body.len() - RECORD_PREFIX_LEN;
    validate_payload_length(declared_payload_len, actual_payload_len)?;

    Ok(())
}

fn validate_payload_length(
    declared_payload_len: usize,
    actual_payload_len: usize,
) -> Result<(), Error> {
    if declared_payload_len > MAX_STORED_PAYLOAD_BYTES {
        return Err(Error::corrupt_data(
            "storage value record exceeds the physical record size limit",
        ));
    }
    if declared_payload_len != actual_payload_len {
        return Err(Error::corrupt_data(format!(
            "storage value record payload length is {declared_payload_len}, actual length is {actual_payload_len}"
        )));
    }

    Ok(())
}

fn read_u32(bytes: &[u8]) -> Result<u32, Error> {
    if bytes.len() != size_of::<u32>() {
        return Err(Error::corrupt_data("invalid encoded u32 length"));
    }
    let mut value = [0_u8; size_of::<u32>()];
    value.copy_from_slice(bytes);
    Ok(u32::from_be_bytes(value))
}

fn read_u64(bytes: &[u8]) -> Result<u64, Error> {
    if bytes.len() != size_of::<u64>() {
        return Err(Error::corrupt_data("invalid encoded u64 length"));
    }
    let mut value = [0_u8; size_of::<u64>()];
    value.copy_from_slice(bytes);
    Ok(u64::from_be_bytes(value))
}

fn invalid_request(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn other(message: impl Into<String>) -> Error {
    Error {
        kind: ErrorKind::Other,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_frames_keep_their_exact_bytes() -> Result<(), Error> {
        let value_frame = b"W3KV\0\0\0\0\0\0\0\x2a\0\0\0\0\0\0\0\x03\0\x01\xff";
        let version_frame = b"W3GV\0\0\0\0\0\0\0\x2a";
        let value = Value(vec![0, 1, 255]);

        assert_eq!(encode_stored_value(&value, Version(42))?, value_frame);
        assert_eq!(
            StoredValue::from(StoredRecord::try_from(value_frame.as_slice())?),
            StoredValue {
                value,
                version: Version(42)
            }
        );
        assert_eq!(encode_global_version(Version(42))?, version_frame);
        assert_eq!(
            Version::from(GlobalVersion::try_from(version_frame.as_slice())?),
            Version(42)
        );
        Ok(())
    }

    #[test]
    fn malformed_global_versions_keep_corruption_classification_and_messages() {
        for (frame, expected) in [
            (
                b"W3G".as_slice(),
                "global version frame is shorter than its header",
            ),
            (
                b"bad!".as_slice(),
                "global version frame has invalid magic bytes",
            ),
            (
                b"W3GV".as_slice(),
                "global version record has length 0, expected 8",
            ),
            (
                b"W3GV\0\0\0\0\0\0\0\0".as_slice(),
                "global version record has an invalid commit version",
            ),
            (
                b"W3GV\0\0\0\0\0\0\0\x01\0".as_slice(),
                "global version record has length 9, expected 8",
            ),
        ] {
            let error =
                GlobalVersion::try_from(frame).expect_err("malformed metadata must fail closed");
            assert_eq!(error.kind, ErrorKind::CorruptData);
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn zero_record_version_is_rejected() {
        let frame = b"W3KV\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        let error = StoredRecord::try_from(frame.as_slice())
            .expect_err("persisted commit version zero must fail closed");
        assert_eq!(error.kind, ErrorKind::CorruptData);
        assert_eq!(
            error.message,
            "storage value record has an invalid commit version"
        );
    }

    #[test]
    fn physical_keys_reject_corruption_and_cross_namespace_reads() {
        let namespace = Namespace("a".to_owned());
        for (physical, expected) in [
            (
                b"\0\0\0".as_slice(),
                "physical key is shorter than its header",
            ),
            (
                b"\0\0\0\x02a".as_slice(),
                "physical key namespace length exceeds the encoded key",
            ),
            (
                b"\0\0\0\x01bkey".as_slice(),
                "physical key does not belong to the requested namespace",
            ),
        ] {
            let error = decode_physical_key(physical, &namespace)
                .expect_err("invalid physical key must not yield a logical key");
            assert_eq!(error.kind, ErrorKind::CorruptData);
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn physical_key_round_trips() -> Result<(), Error> {
        let namespace = Namespace("observations/chain-a-mainnet".to_owned());
        let logical = Key(vec![0, 1, 2, 255]);

        let physical = encode_physical_key(&namespace, &logical)?;

        assert_eq!(decode_physical_key(&physical, &namespace)?, logical);
        Ok(())
    }

    #[test]
    fn stored_value_round_trips() -> Result<(), Error> {
        let value = Value(vec![0, 1, 2, 3, 255]);

        let encoded = encode_stored_value(&value, Version(42))?;

        assert_eq!(
            StoredValue::from(StoredRecord::try_from(encoded.as_slice())?),
            StoredValue {
                value,
                version: Version(42),
            }
        );
        Ok(())
    }

    #[test]
    fn malformed_value_length_is_rejected() -> Result<(), Error> {
        let mut encoded = encode_stored_value(&Value(vec![1, 2, 3]), Version(1))?;
        encoded.push(4);

        let error = StoredRecord::try_from(encoded.as_slice())
            .expect_err("a frame with trailing payload bytes must be rejected");

        assert_eq!(error.kind, ErrorKind::CorruptData);
        assert_eq!(
            error.message,
            "storage value record payload length is 3, actual length is 4"
        );
        Ok(())
    }

    #[test]
    fn invalid_value_magic_is_rejected() -> Result<(), Error> {
        let mut encoded = encode_stored_value(&Value(vec![1]), Version(1))?;
        encoded[0] ^= 0xff;

        let error = StoredRecord::try_from(encoded.as_slice())
            .expect_err("invalid value magic must be rejected");

        assert_eq!(error.kind, ErrorKind::CorruptData);
        assert_eq!(error.message, "storage value frame has invalid magic bytes");
        Ok(())
    }

    #[test]
    fn declared_payload_above_the_corruption_limit_is_rejected_without_allocation() {
        let error = validate_payload_length(MAX_STORED_PAYLOAD_BYTES + 1, 0)
            .expect_err("oversized declared payload must fail closed");
        assert_eq!(error.kind, ErrorKind::CorruptData);
    }

    #[test]
    fn global_version_round_trips() -> Result<(), Error> {
        let encoded = encode_global_version(Version(7))?;
        assert_eq!(
            Version::from(GlobalVersion::try_from(encoded.as_slice())?),
            Version(7)
        );
        Ok(())
    }
}
