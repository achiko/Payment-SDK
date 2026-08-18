use super::*;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum LedgerCauseRecord {
    Opened {
        idempotency_key: String,
    },
    Accounting {
        idempotency_key: String,
        reason: String,
    },
    ReconciliationResolution {
        case_id: String,
        idempotency_key: String,
        reason: String,
    },
    Observation {
        projection_id: String,
        event_id: String,
        revision: u64,
        status: StatusRecord,
        kind: u8,
        movement_ids: Vec<String>,
        network_fee: Option<[u8; 32]>,
    },
}

// design-lint: allow unclassified-free-function -- preserves the frozen ledger cause wire tag
fn ledger_kind_to_tag(kind: LedgerObservationKind) -> u8 {
    match kind {
        LedgerObservationKind::Incoming => 0,
        LedgerObservationKind::Collection => 1,
        LedgerObservationKind::GasFunding => 2,
        LedgerObservationKind::OtherBalanceChange => 3,
    }
}

fn ledger_kind_from_tag(tag: u8) -> Result<LedgerObservationKind, DepositError> {
    match tag {
        0 => Ok(LedgerObservationKind::Incoming),
        1 => Ok(LedgerObservationKind::Collection),
        2 => Ok(LedgerObservationKind::GasFunding),
        3 => Ok(LedgerObservationKind::OtherBalanceChange),
        _ => Err(storage_error(
            "PS ledger record has an unknown observation kind",
        )),
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct LedgerRecord {
    pub(super) version: u16,
    pub(super) id: String,
    pub(super) deposit_id: String,
    pub(super) previous: Option<String>,
    pub(super) cause: LedgerCauseRecord,
    pub(super) balances: BalancesRecord,
    pub(super) recorded_at: u64,
}

impl From<&LedgerEntry> for LedgerRecord {
    fn from(value: &LedgerEntry) -> Self {
        let cause = match &value.cause {
            LedgerEntryCause::Opened { idempotency_key } => LedgerCauseRecord::Opened {
                idempotency_key: idempotency_key.0.clone(),
            },
            LedgerEntryCause::Observation {
                projection_id,
                event_id,
                observation_revision,
                status,
                kind,
                movement_ids,
                network_fee,
            } => LedgerCauseRecord::Observation {
                projection_id: projection_id.0.clone(),
                event_id: event_id.0.clone(),
                revision: observation_revision.0,
                status: status.into(),
                kind: ledger_kind_to_tag(*kind),
                movement_ids: movement_ids.iter().map(|id| id.0.clone()).collect(),
                network_fee: network_fee.as_ref().map(amount::record_bytes),
            },
            LedgerEntryCause::Accounting {
                idempotency_key,
                reason,
            } => LedgerCauseRecord::Accounting {
                idempotency_key: idempotency_key.0.clone(),
                reason: reason.clone(),
            },
            LedgerEntryCause::ReconciliationResolution {
                case_id,
                idempotency_key,
                reason,
            } => LedgerCauseRecord::ReconciliationResolution {
                case_id: case_id.0.clone(),
                idempotency_key: idempotency_key.0.clone(),
                reason: reason.clone(),
            },
        };
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            deposit_id: value.deposit_id.0.clone(),
            previous: value.previous.as_ref().map(|id| id.0.clone()),
            cause,
            balances: value.balances.clone().into(),
            recorded_at: value.recorded_at,
        }
    }
}

impl TryFrom<LedgerRecord> for LedgerEntry {
    type Error = DepositError;

    fn try_from(value: LedgerRecord) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        let cause = match value.cause {
            LedgerCauseRecord::Opened { idempotency_key } => LedgerEntryCause::Opened {
                idempotency_key: IdempotencyKey(idempotency_key),
            },
            LedgerCauseRecord::Accounting {
                idempotency_key,
                reason,
            } => LedgerEntryCause::Accounting {
                idempotency_key: IdempotencyKey(idempotency_key),
                reason,
            },
            LedgerCauseRecord::ReconciliationResolution {
                case_id,
                idempotency_key,
                reason,
            } => LedgerEntryCause::ReconciliationResolution {
                case_id: CaseId(case_id),
                idempotency_key: IdempotencyKey(idempotency_key),
                reason,
            },
            LedgerCauseRecord::Observation {
                projection_id,
                event_id,
                revision,
                status,
                kind,
                movement_ids,
                network_fee,
            } => LedgerEntryCause::Observation {
                projection_id: ProjectionId(projection_id),
                event_id: EventId(event_id),
                observation_revision: ObservationRevision(revision),
                status: status.into(),
                kind: ledger_kind_from_tag(kind)?,
                movement_ids: movement_ids.into_iter().map(MovementId).collect(),
                network_fee: network_fee.map(amount::from_bytes),
            },
        };
        Ok(Self {
            id: EntryId(value.id),
            deposit_id: DepositId(value.deposit_id),
            previous: value.previous.map(EntryId),
            cause,
            balances: value.balances.into(),
            recorded_at: value.recorded_at,
        })
    }
}
