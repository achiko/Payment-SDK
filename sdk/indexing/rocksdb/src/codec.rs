use indexing::IndexError;

use crate::ProjectionBatch;

pub trait RecordTypes: Send + Sync {
    type Target: Clone + Send + Sync + 'static;
    type Effect: Clone + Send + Sync + 'static;
    type Undo: Clone + Send + Sync + 'static;
}

/// Converts a typed chain effect into this adapter's private records.
pub trait Projector: RecordTypes {
    fn project(&self, effect: &Self::Effect) -> Result<ProjectionBatch, IndexError>;
}

pub trait TargetCodec: RecordTypes {
    fn encode_target(&self, target: &Self::Target) -> Result<Vec<u8>, IndexError>;

    fn decode_target(&self, encoded: &[u8]) -> Result<Self::Target, IndexError>;
}

pub trait UndoCodec: RecordTypes {
    fn encode_undo(&self, undo: &Self::Undo) -> Result<Vec<u8>, IndexError>;

    fn decode_undo(&self, encoded: &[u8]) -> Result<Self::Undo, IndexError>;

    fn rollback_projection(&self, _undo: &Self::Undo) -> Result<ProjectionBatch, IndexError> {
        Ok(ProjectionBatch::default())
    }
}

pub trait RecordCodec: TargetCodec + Projector + UndoCodec {}

impl<T> RecordCodec for T where T: TargetCodec + Projector + UndoCodec {}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub enum RawCodec {
    #[default]
    Bytes,
}

#[cfg(test)]
impl RecordTypes for RawCodec {
    type Target = Vec<u8>;
    type Effect = ProjectionBatch;
    type Undo = Vec<u8>;
}

#[cfg(test)]
impl Projector for RawCodec {
    fn project(&self, effect: &Self::Effect) -> Result<ProjectionBatch, IndexError> {
        Ok(effect.clone())
    }
}

#[cfg(test)]
impl TargetCodec for RawCodec {
    fn encode_target(&self, target: &Self::Target) -> Result<Vec<u8>, IndexError> {
        Ok(target.clone())
    }

    fn decode_target(&self, encoded: &[u8]) -> Result<Self::Target, IndexError> {
        Ok(encoded.to_vec())
    }
}

#[cfg(test)]
impl UndoCodec for RawCodec {
    fn encode_undo(&self, undo: &Self::Undo) -> Result<Vec<u8>, IndexError> {
        Ok(undo.clone())
    }

    fn decode_undo(&self, encoded: &[u8]) -> Result<Self::Undo, IndexError> {
        Ok(encoded.to_vec())
    }
}
