use indexing::{
    AssetId, BlockHeight, CanonicalAddress, ChainId, IndexChanges, IndexError, IndexErrorKind,
    IndexUndo, IndexedOutput, OutputId, OutputKey, TransactionRef, WatchSelector,
};

use crate::{ProjectionBatch, ProjectionMutation};

const RECORD_MAGIC: &[u8; 4] = b"IXRC";
const OUTPUT_MAGIC: &[u8; 4] = b"IXOP";
const VALUE_MAGIC: &[u8; 4] = b"IXOV";
const SPENT_MAGIC: &[u8; 4] = b"IXOS";
const RECORD_ENCODING: u8 = 1;
const VALUE_ENCODING: u8 = 1;
const TARGET_RECORD: u8 = 1;
const UNDO_RECORD: u8 = 2;
const CREATED_OUTPUT: u8 = 1;
const SPENT_OUTPUT: u8 = 2;

pub(crate) fn output_prefix(address: &CanonicalAddress) -> Result<Vec<u8>, IndexError> {
    let mut encoded = Vec::new();
    write_output_header(&mut encoded, CREATED_OUTPUT);
    write_address(&mut encoded, address)?;
    Ok(encoded)
}

pub(crate) fn spent_key(key: &OutputKey) -> Result<Vec<u8>, IndexError> {
    output_key(key, SPENT_OUTPUT)
}

pub(crate) fn decode_output(key: &[u8], value: &[u8]) -> Result<IndexedOutput, IndexError> {
    let (kind, key) = decode_output_key(key)?;
    if kind != CREATED_OUTPUT {
        return Err(record_error("output value uses a non-creation key"));
    }
    decode_output_value(key, value)
}

pub(crate) fn decode_spent(key: &[u8], value: &[u8]) -> Result<OutputKey, IndexError> {
    let (kind, key) = decode_output_key(key)?;
    if kind != SPENT_OUTPUT || value != [SPENT_MAGIC.as_slice(), &[RECORD_ENCODING]].concat() {
        return Err(record_error("spent-output marker is invalid"));
    }
    Ok(key)
}
pub(crate) fn project(effect: &IndexChanges) -> Result<ProjectionBatch, IndexError> {
    let outputs = &effect.outputs;
    let mut mutations = Vec::with_capacity(
        outputs
            .created
            .len()
            .checked_add(outputs.spent.len())
            .and_then(|count| count.checked_add(outputs.tracked_spends.len()))
            .ok_or_else(|| record_error("output projection count overflowed"))?,
    );
    for output in &outputs.created {
        mutations.push(ProjectionMutation::Put {
            key: output_key(&output.key(), CREATED_OUTPUT)?,
            value: encode_output(output)?,
        });
    }
    let marker = [SPENT_MAGIC.as_slice(), &[RECORD_ENCODING]].concat();
    for key in &outputs.spent {
        mutations.push(ProjectionMutation::Put {
            key: output_key(key, SPENT_OUTPUT)?,
            value: marker.clone(),
        });
    }
    for key in &outputs.tracked_spends {
        mutations.push(ProjectionMutation::PutIfPresent {
            required_key: output_key(key, CREATED_OUTPUT)?,
            key: output_key(key, SPENT_OUTPUT)?,
            value: marker.clone(),
        });
    }
    Ok(ProjectionBatch::new(mutations))
}
pub(crate) fn encode_target(target: &WatchSelector) -> Result<Vec<u8>, IndexError> {
    let mut encoded = Vec::new();
    write_record_header(&mut encoded, TARGET_RECORD);
    write_address(&mut encoded, target)?;
    Ok(encoded)
}

pub(crate) fn decode_target(encoded: &[u8]) -> Result<WatchSelector, IndexError> {
    let mut reader = Reader::new(record_payload(encoded, TARGET_RECORD)?);
    let target = reader.address()?;
    reader.finish()?;
    Ok(target)
}
pub(crate) fn encode_undo(undo: &IndexUndo) -> Result<Vec<u8>, IndexError> {
    let mut encoded = Vec::new();
    write_record_header(&mut encoded, UNDO_RECORD);
    write_keys(&mut encoded, &undo.created)?;
    write_keys(&mut encoded, &undo.spent)?;
    Ok(encoded)
}

pub(crate) fn decode_undo(encoded: &[u8]) -> Result<IndexUndo, IndexError> {
    let mut reader = Reader::new(record_payload(encoded, UNDO_RECORD)?);
    let undo = IndexUndo {
        created: reader.keys()?,
        spent: reader.keys()?,
    };
    reader.finish()?;
    Ok(undo)
}

pub(crate) fn rollback_projection(undo: &IndexUndo) -> Result<ProjectionBatch, IndexError> {
    let spent = undo.spent.iter().map(|key| {
        Ok(ProjectionMutation::Delete {
            key: output_key(key, SPENT_OUTPUT)?,
        })
    });
    let created = undo.created.iter().map(|key| {
        Ok(ProjectionMutation::Delete {
            key: output_key(key, CREATED_OUTPUT)?,
        })
    });
    Ok(ProjectionBatch::new(
        spent
            .chain(created)
            .collect::<Result<Vec<_>, IndexError>>()?,
    ))
}
fn output_key(key: &OutputKey, kind: u8) -> Result<Vec<u8>, IndexError> {
    let mut encoded = Vec::new();
    write_output_header(&mut encoded, kind);
    write_address(&mut encoded, &key.address)?;
    write_transaction(&mut encoded, &key.output.transaction)?;
    encoded.extend_from_slice(&key.output.index.to_be_bytes());
    Ok(encoded)
}

fn decode_output_key(encoded: &[u8]) -> Result<(u8, OutputKey), IndexError> {
    if encoded.len() < 6
        || encoded.get(..4) != Some(OUTPUT_MAGIC.as_slice())
        || encoded[4] != RECORD_ENCODING
    {
        return Err(record_error("output key header is invalid"));
    }
    let kind = encoded[5];
    let mut reader = Reader::new(&encoded[6..]);
    let address = reader.address()?;
    let transaction = reader.transaction()?;
    let index = u32::from_be_bytes(reader.array()?);
    reader.finish()?;
    Ok((
        kind,
        OutputKey {
            address,
            output: OutputId { transaction, index },
        },
    ))
}

fn encode_output(output: &IndexedOutput) -> Result<Vec<u8>, IndexError> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(VALUE_MAGIC);
    encoded.push(VALUE_ENCODING);
    write_text(&mut encoded, &crate::amount_record::encode(&output.amount))?;
    encoded.extend_from_slice(&output.created_at.0.to_be_bytes());
    encoded.push(u8::from(output.coinbase));
    write_asset(&mut encoded, &output.asset)?;
    write_bytes(&mut encoded, &output.evidence)?;
    Ok(encoded)
}

fn decode_output_value(key: OutputKey, encoded: &[u8]) -> Result<IndexedOutput, IndexError> {
    if encoded.len() < 5
        || encoded.get(..4) != Some(VALUE_MAGIC.as_slice())
        || encoded[4] != VALUE_ENCODING
    {
        return Err(record_error("output value header is invalid"));
    }
    let mut reader = Reader::new(&encoded[5..]);
    let amount = crate::amount_record::decode(&reader.text()?)?;
    let created_at = BlockHeight(u64::from_be_bytes(reader.array()?));
    let coinbase = match reader.byte()? {
        0 => false,
        1 => true,
        _ => return Err(record_error("output coinbase flag is invalid")),
    };
    let asset = reader.asset()?;
    let evidence = reader.bytes()?.to_vec();
    reader.finish()?;
    Ok(IndexedOutput {
        id: key.output,
        address: key.address,
        asset,
        amount,
        evidence,
        created_at,
        coinbase,
    })
}

fn write_record_header(encoded: &mut Vec<u8>, kind: u8) {
    encoded.extend_from_slice(RECORD_MAGIC);
    encoded.push(RECORD_ENCODING);
    encoded.push(kind);
}

fn write_output_header(encoded: &mut Vec<u8>, kind: u8) {
    encoded.extend_from_slice(OUTPUT_MAGIC);
    encoded.push(RECORD_ENCODING);
    encoded.push(kind);
}

fn record_payload(encoded: &[u8], kind: u8) -> Result<&[u8], IndexError> {
    if encoded.len() < 6
        || encoded.get(..4) != Some(RECORD_MAGIC.as_slice())
        || encoded[4] != RECORD_ENCODING
        || encoded[5] != kind
    {
        return Err(record_error("index record header is invalid"));
    }
    Ok(&encoded[6..])
}

fn write_address(encoded: &mut Vec<u8>, address: &CanonicalAddress) -> Result<(), IndexError> {
    write_text(encoded, &address.scope.chain.0)?;
    write_text(encoded, &address.scope.network)?;
    write_text(encoded, &address.value)
}

fn write_transaction(
    encoded: &mut Vec<u8>,
    transaction: &TransactionRef,
) -> Result<(), IndexError> {
    write_text(encoded, &transaction.scope.chain.0)?;
    write_text(encoded, &transaction.scope.network)?;
    write_text(encoded, &transaction.value)
}

fn write_asset(encoded: &mut Vec<u8>, asset: &AssetId) -> Result<(), IndexError> {
    write_text(encoded, &asset.chain.0)?;
    write_text(encoded, &asset.asset)
}

fn write_text(encoded: &mut Vec<u8>, value: &str) -> Result<(), IndexError> {
    write_bytes(encoded, value.as_bytes())
}

fn write_bytes(encoded: &mut Vec<u8>, value: &[u8]) -> Result<(), IndexError> {
    let length =
        u32::try_from(value.len()).map_err(|_| record_error("record field is too long"))?;
    encoded.extend_from_slice(&length.to_be_bytes());
    encoded.extend_from_slice(value);
    Ok(())
}

fn write_keys(encoded: &mut Vec<u8>, keys: &[OutputKey]) -> Result<(), IndexError> {
    let count =
        u32::try_from(keys.len()).map_err(|_| record_error("undo contains too many keys"))?;
    encoded.extend_from_slice(&count.to_be_bytes());
    for key in keys {
        write_address(encoded, &key.address)?;
        write_transaction(encoded, &key.output.transaction)?;
        encoded.extend_from_slice(&key.output.index.to_be_bytes());
    }
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

    fn array<const N: usize>(&mut self) -> Result<[u8; N], IndexError> {
        self.take(N)?
            .try_into()
            .map_err(|_| record_error("record field has an invalid length"))
    }

    fn byte(&mut self) -> Result<u8, IndexError> {
        Ok(self.take(1)?[0])
    }

    fn bytes(&mut self) -> Result<&'a [u8], IndexError> {
        let length = usize::try_from(u32::from_be_bytes(self.array()?))
            .map_err(|_| record_error("record length is unsupported"))?;
        self.take(length)
    }

    fn text(&mut self) -> Result<String, IndexError> {
        std::str::from_utf8(self.bytes()?)
            .map(str::to_owned)
            .map_err(|_| record_error("record text is not UTF-8"))
    }

    fn address(&mut self) -> Result<CanonicalAddress, IndexError> {
        Ok(CanonicalAddress {
            scope: indexing::IndexScope {
                chain: ChainId(self.text()?),
                network: self.text()?,
            },
            value: self.text()?,
        })
    }

    fn transaction(&mut self) -> Result<TransactionRef, IndexError> {
        Ok(TransactionRef {
            scope: indexing::IndexScope {
                chain: ChainId(self.text()?),
                network: self.text()?,
            },
            value: self.text()?,
        })
    }

    fn asset(&mut self) -> Result<AssetId, IndexError> {
        Ok(AssetId {
            chain: ChainId(self.text()?),
            asset: self.text()?,
        })
    }

    fn keys(&mut self) -> Result<Vec<OutputKey>, IndexError> {
        let count = usize::try_from(u32::from_be_bytes(self.array()?))
            .map_err(|_| record_error("undo key count is unsupported"))?;
        let mut keys = Vec::with_capacity(count);
        for _ in 0..count {
            keys.push(OutputKey {
                address: self.address()?,
                output: OutputId {
                    transaction: self.transaction()?,
                    index: u32::from_be_bytes(self.array()?),
                },
            });
        }
        Ok(keys)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], IndexError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| record_error("record length overflowed"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| record_error("record is truncated"))?;
        self.position = end;
        Ok(value)
    }

    fn finish(&self) -> Result<(), IndexError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(record_error("record has trailing bytes"))
        }
    }
}

fn record_error(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

#[cfg(test)]
#[path = "index_record_test.rs"]
mod tests;
