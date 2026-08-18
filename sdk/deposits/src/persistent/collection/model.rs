use super::*;

impl Collection {
    pub(super) fn from_create(command: &CollectionPlan) -> Result<Self, DepositError> {
        command.validate()?;
        let mut legs = Vec::with_capacity(command.legs.len());
        for (position, leg) in command.legs.iter().enumerate() {
            let position = u16::try_from(position)
                .map_err(|_| invalid("collection contains too many ordered legs"))?;
            legs.push(CollectionLeg {
                id: leg.id.clone(),
                position,
                kind: leg.kind,
                planned_amount: leg.planned_amount.clone(),
                state: CollectionLegState::Required,
                watch_id: None,
                attempt_count: 0,
                allocation: None,
                allocations: Vec::new(),
                last_error: None,
                updated_at: command.created_at,
            });
        }
        let reservation = CollectionReservation {
            deposit_id: command.deposit_id.clone(),
            asset: command.asset.clone(),
            amount: command.reservation_amount.clone(),
            state: CollectionReservationState::Active,
        };
        Ok(Collection {
            id: command.id.clone(),
            job_id: command.job_id.clone(),
            mode: command.mode,
            asset: command.asset.clone(),
            destination: command.destination.clone(),
            policy: command.policy.clone(),
            state: CollectionState::Required,
            participants: vec![CollectionParticipant {
                user_id: command.user_id.clone(),
                reservation,
                spend_resources: Vec::new(),
            }],
            legs,
            attempt_count: 0,
            last_error: None,
            created_at: command.created_at,
            updated_at: command.created_at,
        })
    }

    pub(super) fn from_batch(command: &CreateBatch) -> Result<Self, DepositError> {
        command.validate()?;
        let participants = command
            .participants
            .iter()
            .map(|participant| CollectionParticipant {
                user_id: participant.user_id.clone(),
                reservation: CollectionReservation {
                    deposit_id: participant.deposit_id.clone(),
                    asset: command.asset.clone(),
                    amount: participant.reservation_amount.clone(),
                    state: CollectionReservationState::Active,
                },
                spend_resources: participant.spend_resources.clone(),
            })
            .collect::<Vec<_>>();
        Ok(Collection {
            id: command.id.clone(),
            job_id: command.job_id.clone(),
            mode: CollectionMode::UtxoBatch,
            asset: command.asset.clone(),
            destination: command.destination.clone(),
            policy: command.policy.clone(),
            state: CollectionState::Required,
            participants,
            legs: vec![CollectionLeg {
                id: command.leg.id.clone(),
                position: 0,
                kind: CollectionLegKind::Sweep,
                planned_amount: None,
                state: CollectionLegState::Required,
                watch_id: None,
                attempt_count: 0,
                allocation: None,
                allocations: Vec::new(),
                last_error: None,
                updated_at: command.created_at,
            }],
            attempt_count: 0,
            last_error: None,
            created_at: command.created_at,
            updated_at: command.created_at,
        })
    }
}

impl CollectionQuery {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        let request = self;
        if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
            Err(invalid("collection page size must be between 1 and 1000"))
        } else {
            Ok(())
        }
    }
}

pub(super) fn validate_guard(
    collection: &Collection,
    leg: &CollectionLeg,
    guard: &TransitionGuard,
) -> Result<(), DepositError> {
    if collection.state != guard.collection_state {
        return Err(conflict("stale expected collection aggregate state"));
    }
    if leg.state != guard.leg_state {
        return Err(conflict("stale expected collection leg state"));
    }
    Ok(())
}

pub(super) fn validate_transition_time(
    collection: &Collection,
    updated_at: u64,
) -> Result<(), DepositError> {
    if updated_at < collection.updated_at {
        Err(invalid("collection transition timestamp moved backwards"))
    } else {
        Ok(())
    }
}

pub(super) fn find_leg(collection: &Collection, leg_id: &LegId) -> Result<usize, DepositError> {
    collection
        .legs
        .iter()
        .position(|leg| &leg.id == leg_id)
        .ok_or_else(|| not_found("collection leg was not found"))
}

pub(super) fn ensure_previous_legs_confirmed(
    collection: &Collection,
    position: usize,
) -> Result<(), DepositError> {
    if collection.legs[..position]
        .iter()
        .all(|leg| matches!(leg.state, CollectionLegState::Confirmed { .. }))
    {
        Ok(())
    } else {
        Err(invalid_state(
            "collection leg cannot advance before all previous legs are confirmed",
        ))
    }
}

impl Collection {
    pub(super) fn all_legs_confirmed(&self) -> bool {
        self.legs
            .iter()
            .all(|leg| matches!(leg.state, CollectionLegState::Confirmed { .. }))
    }
}

pub(super) fn set_reservation_state(
    collection: &mut Collection,
    state: CollectionReservationState,
) {
    for participant in &mut collection.participants {
        participant.reservation.state = state.clone();
    }
}

pub(crate) struct BatchTransition {
    pub(crate) collection: Collection,
    pub(crate) conditions: Vec<Condition>,
    pub(crate) operations: Vec<Operation>,
}
