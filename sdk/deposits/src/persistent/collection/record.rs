use super::*;
use crate::persistent::AddressRecord;

// design-lint: allow duplicate-entity-base -- current collection is an independent aggregate schema
#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct StoredRecord {
    pub(super) version: u16,
    pub(super) id: String,
    pub(super) job_id: String,
    pub(super) mode: u8,
    pub(super) asset: AssetRecord,
    pub(super) destination: AddressRecord,
    pub(super) policy: PolicyRecord,
    pub(super) state: u8,
    pub(super) participants: Vec<ParticipantRecord>,
    pub(super) legs: Vec<LegRecord>,
    pub(super) attempt_count: u32,
    pub(super) last_error: Option<FailureRecord>,
    pub(super) created_at: u64,
    pub(super) updated_at: u64,
}

impl From<&Collection> for StoredRecord {
    fn from(value: &Collection) -> Self {
        Self {
            version: COLLECTION_RECORD_VERSION,
            id: value.id.0.clone(),
            job_id: value.job_id.0.clone(),
            mode: mode_tag(value.mode),
            asset: (&value.asset).into(),
            destination: (&value.destination).into(),
            policy: (&value.policy).into(),
            state: state_tag(value.state),
            participants: value.participants.iter().map(Into::into).collect(),
            legs: value.legs.iter().map(Into::into).collect(),
            attempt_count: value.attempt_count,
            last_error: value.last_error.as_ref().map(Into::into),
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

impl TryFrom<StoredRecord> for Collection {
    type Error = DepositError;

    fn try_from(value: StoredRecord) -> Result<Self, Self::Error> {
        if value.version != COLLECTION_RECORD_VERSION {
            return Err(storage_error(format!(
                "unsupported PS collection record version {}",
                value.version
            )));
        }
        let participants = value
            .participants
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<CollectionParticipant>, _>>()?;
        let collection = Self {
            id: CollectionId(value.id),
            job_id: JobId(value.job_id),
            mode: mode_from_tag(value.mode)?,
            asset: value.asset.into(),
            destination: value.destination.into(),
            policy: value.policy.into(),
            state: state_from_tag(value.state)?,
            participants,
            legs: value
                .legs
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            attempt_count: value.attempt_count,
            last_error: value.last_error.map(Into::into),
            created_at: value.created_at,
            updated_at: value.updated_at,
        };
        collection.validate_persisted()?;
        Ok(collection)
    }
}

// design-lint: allow unclassified-free-function -- preserves the frozen collection mode wire tag
fn mode_tag(mode: CollectionMode) -> u8 {
    match mode {
        CollectionMode::AccountTransfer => 0,
        CollectionMode::UtxoBatch => 1,
        CollectionMode::TokenWithGas => 2,
    }
}

fn mode_from_tag(tag: u8) -> Result<CollectionMode, DepositError> {
    match tag {
        0 => Ok(CollectionMode::AccountTransfer),
        1 => Ok(CollectionMode::UtxoBatch),
        2 => Ok(CollectionMode::TokenWithGas),
        _ => Err(storage_error(
            "PS collection record has an unknown collection mode",
        )),
    }
}

// design-lint: allow unclassified-free-function -- preserves the frozen collection state wire tag
fn state_tag(state: CollectionState) -> u8 {
    match state {
        CollectionState::Required => 0,
        CollectionState::InProgress => 1,
        CollectionState::Completed => 2,
        CollectionState::Failed => 3,
        CollectionState::Reorged => 4,
    }
}

fn state_from_tag(tag: u8) -> Result<CollectionState, DepositError> {
    match tag {
        0 => Ok(CollectionState::Required),
        1 => Ok(CollectionState::InProgress),
        2 => Ok(CollectionState::Completed),
        3 => Ok(CollectionState::Failed),
        4 => Ok(CollectionState::Reorged),
        _ => Err(storage_error(
            "PS collection record has an unknown aggregate state",
        )),
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct IndexRecord {
    pub(super) version: u16,
    pub(super) collection_id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct LegIndex {
    pub(super) version: u16,
    pub(super) collection_id: String,
    pub(super) leg_id: String,
}

/// Intentionally does not derive `Debug`: its opaque bytes must not enter
/// diagnostic output even inside this persistence implementation.
#[derive(Clone, Decode, Encode, PartialEq, Eq)]
pub(super) struct EnvelopeRecord {
    pub(super) version: u16,
    pub(super) collection_id: String,
    pub(super) leg_id: String,
    pub(super) expected_transaction_id: TransactionRecord,
    pub(super) bytes: Vec<u8>,
    pub(super) signed_at: u64,
    pub(super) expires_at: u64,
}

impl From<&SignedEnvelope> for EnvelopeRecord {
    fn from(value: &SignedEnvelope) -> Self {
        Self {
            version: RECORD_VERSION,
            collection_id: value.collection_id.0.clone(),
            leg_id: value.leg_id.0.clone(),
            expected_transaction_id: (&value.expected_transaction_id).into(),
            bytes: value.bytes.as_bytes().to_vec(),
            signed_at: value.signed_at,
            expires_at: value.expires_at,
        }
    }
}

impl TryFrom<EnvelopeRecord> for SignedEnvelope {
    type Error = DepositError;

    fn try_from(value: EnvelopeRecord) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        Ok(Self {
            collection_id: CollectionId(value.collection_id),
            leg_id: LegId(value.leg_id),
            expected_transaction_id: value.expected_transaction_id.into(),
            bytes: SignedBytes::new(value.bytes).map_err(|error| storage_error(error.message))?,
            signed_at: value.signed_at,
            expires_at: value.expires_at,
        })
    }
}
