use deposits::{
    BalanceDirection, CaseId, Collection, CollectionError, CollectionLeg, CollectionMode,
    CollectionReservationState, Deposit, DepositError, DepositErrorKind, DepositId, LedgerEffect,
    MirroredObservation, ReconciliationCase, ReconciliationReason, ReconciliationState,
    ReleaseReservation, ReservationReleaseReason, TransitionGuard,
};
use indexing::{CanonicalAddress, MovementId, ValueMovement};

use super::{
    ObservationStore,
    classification::{Class, Movements},
};

pub(super) async fn release(
    store: &dyn ObservationStore,
    collection: Collection,
    reason: ReservationReleaseReason,
    at: u64,
) -> Result<(), DepositError> {
    if collection.reservation().state == CollectionReservationState::Active {
        store
            .release_reservation(ReleaseReservation {
                collection_id: collection.id,
                expected_collection_state: collection.state,
                expected_reservation_state: CollectionReservationState::Active,
                reason,
                released_at: collection.updated_at.max(at),
            })
            .await?;
    }
    Ok(())
}

pub(super) fn guard(collection: &Collection, leg: &CollectionLeg) -> TransitionGuard {
    TransitionGuard {
        collection_state: collection.state,
        leg_state: leg.state.clone(),
    }
}

pub(super) fn collection_error(code: &str, message: &str) -> CollectionError {
    CollectionError {
        code: code.to_owned(),
        message: message.to_owned(),
        retryable: false,
    }
}

pub(super) async fn matching_deposit(
    store: &dyn ObservationStore,
    address: &CanonicalAddress,
    movement: &ValueMovement,
) -> Result<Option<Deposit>, DepositError> {
    Ok(store
        .by_address(address)
        .await?
        .filter(|deposit| &deposit.asset == movement.asset()))
}

pub(super) async fn append_spend_case(
    store: &dyn ObservationStore,
    observation: &MirroredObservation,
    deposit: &Deposit,
    cases: &mut Vec<ReconciliationCase>,
) -> Result<(), DepositError> {
    let Some(collection) = store
        .retained_collection_for(&deposit.id, &deposit.asset)
        .await?
    else {
        return Ok(());
    };
    let participant = collection
        .participant(&deposit.id)
        .ok_or_else(|| invalid("retained collection omits its indexed deposit"))?;
    if collection.mode != CollectionMode::UtxoBatch
        || participant.reservation.asset != deposit.asset
        || matches!(
            participant.reservation.state,
            CollectionReservationState::Released { .. }
        )
    {
        return Err(invalid(
            "retained collection index points to incompatible ownership",
        ));
    }
    cases.push(ReconciliationCase {
        id: spend_case_id(observation, &deposit.id),
        deposit_id: deposit.id.clone(),
        triggering_event_id: observation.event.id.clone(),
        reason: ReconciliationReason::ReservedSpendConflict {
            collection_id: collection.id,
            transaction_id: observation.event.transaction.transaction_id.clone(),
        },
        state: ReconciliationState::Open,
        created_at: observation.received_at,
    });
    Ok(())
}

impl Movements {
    pub(super) fn effect(&self) -> LedgerEffect<MovementId> {
        if let Some(class) = self.class {
            return match class {
                Class::Collection => LedgerEffect::Collection {
                    movements: self.outgoing.clone(),
                },
                Class::GasFunding => LedgerEffect::GasFunding {
                    movements: self.incoming.clone(),
                },
            };
        }
        match (self.outgoing.is_empty(), self.incoming.is_empty()) {
            (true, false) => LedgerEffect::Incoming {
                movements: self.incoming.clone(),
            },
            (false, true) => LedgerEffect::OtherBalanceChange {
                direction: BalanceDirection::Debit,
                movements: self.outgoing.clone(),
            },
            (false, false) => LedgerEffect::NetBalanceChange {
                debit_movements: self.outgoing.clone(),
                credit_movements: self.incoming.clone(),
            },
            (true, true) => LedgerEffect::OtherBalanceChange {
                direction: BalanceDirection::Debit,
                movements: Vec::new(),
            },
        }
    }
}

pub(super) fn resolve(
    event: &indexing::ObservationEvent,
    effect: &LedgerEffect<MovementId>,
) -> Result<LedgerEffect<base::Decimal>, DepositError> {
    let amounts = |ids: &[MovementId]| {
        ids.iter()
            .map(|id| {
                event
                    .transaction
                    .movements
                    .iter()
                    .find(|movement| movement.id() == id)
                    .map(|movement| movement.amount().clone())
                    .ok_or_else(|| invalid("classified movement disappeared from its IX event"))
            })
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(match effect {
        LedgerEffect::Incoming { movements } => LedgerEffect::Incoming {
            movements: amounts(movements)?,
        },
        LedgerEffect::OtherBalanceChange {
            direction,
            movements,
        } => LedgerEffect::OtherBalanceChange {
            direction: *direction,
            movements: amounts(movements)?,
        },
        LedgerEffect::NetBalanceChange {
            debit_movements,
            credit_movements,
        } => LedgerEffect::NetBalanceChange {
            debit_movements: amounts(debit_movements)?,
            credit_movements: amounts(credit_movements)?,
        },
        LedgerEffect::Collection { movements } => LedgerEffect::Collection {
            movements: amounts(movements)?,
        },
        LedgerEffect::GasFunding { movements } => LedgerEffect::GasFunding {
            movements: amounts(movements)?,
        },
    })
}

pub(super) fn case_id(observation: &MirroredObservation, deposit: &DepositId) -> CaseId {
    CaseId(format!(
        "reorg:{}:{}:{}:{}",
        observation.event.id.0.len(),
        observation.event.id.0,
        deposit.0.len(),
        deposit.0
    ))
}

pub(super) fn spend_case_id(observation: &MirroredObservation, deposit: &DepositId) -> CaseId {
    CaseId(format!(
        "reserved-spend:{}:{}:{}:{}",
        observation.event.id.0.len(),
        observation.event.id.0,
        deposit.0.len(),
        deposit.0
    ))
}

pub(super) fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}
