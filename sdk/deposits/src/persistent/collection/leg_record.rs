use super::*;
use crate::amount;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct SpendRecord {
    pub(super) transaction_id: TransactionRecord,
    pub(super) output_index: u32,
    pub(super) amount: [u8; 32],
    pub(super) evidence: Vec<u8>,
}

impl From<&SpendResource> for SpendRecord {
    fn from(value: &SpendResource) -> Self {
        Self {
            transaction_id: (&value.id.transaction_id).into(),
            output_index: value.id.output_index,
            amount: amount::record_bytes(&value.amount),
            evidence: value.evidence.as_bytes().to_vec(),
        }
    }
}

impl TryFrom<SpendRecord> for SpendResource {
    type Error = DepositError;

    fn try_from(value: SpendRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            id: ResourceId {
                transaction_id: value.transaction_id.into(),
                output_index: value.output_index,
            },
            amount: amount::from_bytes(value.amount),
            evidence: ResourceProof::new(value.evidence)
                .map_err(|error| storage_error(error.message))?,
        })
    }
}

// design-lint: allow duplicate-entity-base -- current participant is a distinct nested wire record
#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ParticipantRecord {
    pub(super) user_id: String,
    pub(super) deposit_id: String,
    pub(super) asset: AssetRecord,
    pub(super) reservation_amount: [u8; 32],
    pub(super) reservation_state: ReservationStateRecord,
    pub(super) spend_resources: Vec<SpendRecord>,
}

impl From<&CollectionParticipant> for ParticipantRecord {
    fn from(value: &CollectionParticipant) -> Self {
        Self {
            user_id: value.user_id.0.clone(),
            deposit_id: value.reservation.deposit_id.0.clone(),
            asset: (&value.reservation.asset).into(),
            reservation_amount: amount::record_bytes(&value.reservation.amount),
            reservation_state: (&value.reservation.state).into(),
            spend_resources: value.spend_resources.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<ParticipantRecord> for CollectionParticipant {
    type Error = DepositError;

    fn try_from(value: ParticipantRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            user_id: UserId(value.user_id),
            reservation: CollectionReservation {
                deposit_id: DepositId(value.deposit_id),
                asset: value.asset.into(),
                amount: amount::from_bytes(value.reservation_amount),
                state: value.reservation_state.try_into()?,
            },
            spend_resources: value
                .spend_resources
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

// design-lint: allow duplicate-entity-base -- current leg evolves independently from current collection
#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct LegRecord {
    pub(super) id: String,
    pub(super) position: u16,
    pub(super) kind: u8,
    pub(super) planned_amount: Option<[u8; 32]>,
    pub(super) state: LegStateRecord,
    pub(super) watch_id: Option<String>,
    pub(super) attempt_count: u32,
    pub(super) allocations: Vec<AllocationRecord>,
    pub(super) last_error: Option<FailureRecord>,
    pub(super) updated_at: u64,
}

impl From<&CollectionLeg> for LegRecord {
    fn from(value: &CollectionLeg) -> Self {
        Self {
            id: value.id.0.clone(),
            position: value.position,
            kind: leg_kind_tag(value.kind),
            planned_amount: value.planned_amount.as_ref().map(amount::record_bytes),
            state: (&value.state).into(),
            watch_id: value.watch_id.as_ref().map(|watch| watch.0.clone()),
            attempt_count: value.attempt_count,
            allocations: value.allocations.iter().map(Into::into).collect(),
            last_error: value.last_error.as_ref().map(Into::into),
            updated_at: value.updated_at,
        }
    }
}

impl TryFrom<LegRecord> for CollectionLeg {
    type Error = DepositError;

    fn try_from(value: LegRecord) -> Result<Self, Self::Error> {
        let allocations = value
            .allocations
            .into_iter()
            .map(CollectionAllocation::from)
            .collect::<Vec<_>>();
        let allocation = (allocations.len() == 1).then(|| allocations[0].clone());
        Ok(Self {
            id: LegId(value.id),
            position: value.position,
            kind: leg_kind_from_tag(value.kind)?,
            planned_amount: value.planned_amount.map(amount::from_bytes),
            state: value.state.into(),
            watch_id: value.watch_id.map(WatchId),
            attempt_count: value.attempt_count,
            allocation,
            allocations,
            last_error: value.last_error.map(Into::into),
            updated_at: value.updated_at,
        })
    }
}

// design-lint: allow unclassified-free-function -- preserves the current wire tag mapping
fn leg_kind_tag(kind: CollectionLegKind) -> u8 {
    match kind {
        CollectionLegKind::GasFunding => 0,
        CollectionLegKind::Sweep => 1,
    }
}

fn leg_kind_from_tag(tag: u8) -> Result<CollectionLegKind, DepositError> {
    match tag {
        0 => Ok(CollectionLegKind::GasFunding),
        1 => Ok(CollectionLegKind::Sweep),
        _ => Err(storage_error(
            "PS collection record has an unknown leg kind",
        )),
    }
}
