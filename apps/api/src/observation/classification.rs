use std::collections::BTreeMap;

use deposits::{
    BatchMutation, Collection, CollectionLeg, CollectionLegKind, CollectionLegState,
    CollectionMode, Deposit, DepositError, DepositId, MirroredObservation, ProjectionFeeTreatment,
    ReconciliationCase, ReconciliationReason, ReconciliationState, RecordObservation,
    ReservationReleaseReason, UtxoBatchProjectionTransition, apply_observation_transition,
};
use indexing::{MovementId, MovementKind, ValueMovement};

use super::facts::{
    append_spend_case, case_id, collection_error, guard, invalid, matching_deposit, release,
    resolve,
};

use super::ObservationStore;

pub(super) struct Projection {
    pub deposits: Vec<DepositId>,
    pub updates: Vec<RecordObservation>,
    pub cases: Vec<ReconciliationCase>,
    pub fees: ProjectionFeeTreatment,
    pub batch: Option<BatchMutation>,
}

#[derive(Default)]
pub(super) struct Movements {
    pub(super) incoming: Vec<MovementId>,
    pub(super) outgoing: Vec<MovementId>,
    pub(super) input: bool,
    pub(super) class: Option<Class>,
}

#[derive(Clone, Copy)]
pub(super) enum Class {
    Collection,
    GasFunding,
}

impl Projection {
    pub(super) async fn classify(
        store: &dyn ObservationStore,
        observation: &MirroredObservation,
    ) -> Result<Self, DepositError> {
        let mut batch = None;
        let mut deposits = if let Some(reference) = store
            .leg_for_transaction(&observation.event.transaction.transaction_id)
            .await?
        {
            let collection = store
                .collection(&reference.collection_id)
                .await?
                .ok_or_else(|| invalid("collection transaction index is dangling"))?;
            let leg = collection
                .legs
                .iter()
                .find(|leg| leg.id == reference.leg_id)
                .ok_or_else(|| invalid("collection transaction points to a missing leg"))?;
            let (classified, mutation) =
                classify_collection(store, observation, &collection, leg).await?;
            batch = mutation;
            classified
        } else {
            BTreeMap::<DepositId, (Deposit, Movements)>::new()
        };
        if deposits.is_empty() && batch.is_none() {
            for movement in &observation.event.transaction.movements {
                record_movement(store, &mut deposits, movement).await?;
            }
        }
        if batch.is_none()
            && let Some(fee) = &observation.event.transaction.fee
            && let Some(payer) = &fee.payer
            && let Some(deposit) = store.by_address(payer).await?
            && deposit.asset == fee.asset
        {
            deposits
                .entry(deposit.id.clone())
                .or_insert_with(|| (deposit, Movements::default()));
        }
        if deposits.is_empty() {
            return Err(invalid(
                "a watched IX event cannot be attributed to a durable deposit",
            ));
        }

        let fee_in_inputs = deposits.values().any(|(_, movements)| movements.input);
        let mut updates = Vec::with_capacity(deposits.len());
        let mut cases = Vec::new();
        for (id, (deposit, movements)) in &deposits {
            if matches!(movements.class, Some(Class::GasFunding))
                && movements.incoming.is_empty()
                && movements.outgoing.is_empty()
            {
                continue;
            }
            let head = store
                .current(id)
                .await?
                .ok_or_else(|| invalid("an observed deposit has no ledger head"))?;
            let effect = movements.effect();
            let transition = deposits::LedgerTransition {
                status: observation.event.transaction.status.clone(),
                previous_status: observation.event.previous_status.clone(),
                effect: resolve(&observation.event, &effect)?,
                network_fee: (!fee_in_inputs)
                    .then(|| {
                        observation.event.transaction.fee.as_ref().and_then(|fee| {
                            (fee.asset == deposit.asset
                                && fee.payer.as_ref() == Some(&deposit.address))
                            .then(|| fee.amount.clone())
                        })
                    })
                    .flatten(),
            };
            let next = apply_observation_transition(head.balances.clone(), &transition).map_err(
                |error| invalid(format!("IX fact cannot update deposit ledger: {error}")),
            )?;
            if head.balances.accounted <= head.balances.confirmed && next.accounted > next.confirmed
            {
                cases.push(ReconciliationCase {
                    id: case_id(observation, id),
                    deposit_id: id.clone(),
                    triggering_event_id: observation.event.id.clone(),
                    reason: ReconciliationReason::PostCreditReorg {
                        accounted: next.accounted,
                        corrected_confirmed: next.confirmed,
                    },
                    state: ReconciliationState::Open,
                    created_at: observation.received_at,
                });
            }
            if movements.class.is_none()
                && movements.input
                && !movements.outgoing.is_empty()
                && matches!(
                    observation.event.transaction.status,
                    indexing::TransactionStatus::Included { .. }
                        | indexing::TransactionStatus::Confirmed { .. }
                )
            {
                append_spend_case(store, observation, deposit, &mut cases).await?;
            }
            updates.push(RecordObservation {
                event_id: observation.event.id.clone(),
                effect,
                deposit_id: id.clone(),
                expected_head: Some(head.id),
                recorded_at: observation.received_at,
            });
        }
        Ok(Projection {
            deposits: deposits.into_keys().collect(),
            updates,
            cases,
            fees: if fee_in_inputs {
                ProjectionFeeTreatment::IncludedInMovementEffect
            } else {
                ProjectionFeeTreatment::Separate
            },
            batch,
        })
    }
}

async fn record_movement(
    store: &dyn ObservationStore,
    deposits: &mut BTreeMap<DepositId, (Deposit, Movements)>,
    movement: &ValueMovement,
) -> Result<(), DepositError> {
    if let Some(address) = movement.to()
        && let Some(deposit) = matching_deposit(store, address, movement).await?
    {
        deposits
            .entry(deposit.id.clone())
            .or_insert_with(|| (deposit, Movements::default()))
            .1
            .incoming
            .push(movement.id().clone());
    }
    if let Some(address) = movement.from()
        && let Some(deposit) = matching_deposit(store, address, movement).await?
    {
        let movements = &mut deposits
            .entry(deposit.id.clone())
            .or_insert_with(|| (deposit, Movements::default()))
            .1;
        movements.outgoing.push(movement.id().clone());
        movements.input |= movement.kind() == MovementKind::Input;
    }
    Ok(())
}

async fn classify_collection(
    store: &dyn ObservationStore,
    observation: &MirroredObservation,
    collection: &Collection,
    leg: &CollectionLeg,
) -> Result<
    (
        BTreeMap<DepositId, (Deposit, Movements)>,
        Option<BatchMutation>,
    ),
    DepositError,
> {
    let transaction = &observation.event.transaction;
    if leg.state.transaction_id() != Some(&transaction.transaction_id) || leg.watch_id.is_none() {
        return Err(invalid(
            "collection IX fact is not bound to its durable watched leg",
        ));
    }
    let transition_at = collection.updated_at.max(observation.received_at);
    let mut deposits = BTreeMap::new();
    if collection.mode == CollectionMode::UtxoBatch {
        for participant in &collection.participants {
            let deposit = store
                .deposit(&participant.reservation.deposit_id)
                .await?
                .ok_or_else(|| invalid("collection participant deposit is missing"))?;
            let outgoing = transaction
                .movements
                .iter()
                .filter(|movement| {
                    movement.kind() == MovementKind::Input
                        && movement.asset() == &deposit.asset
                        && movement.from() == Some(&deposit.address)
                })
                .map(|movement| movement.id().clone())
                .collect::<Vec<_>>();
            if outgoing.is_empty() {
                return Err(invalid("UTXO collection fact omits a participant input"));
            }
            let allocation = leg
                .allocations
                .iter()
                .find(|allocation| allocation.deposit_id == deposit.id)
                .ok_or_else(|| invalid("UTXO collection allocation is missing"))?;
            let debit = sum(&observation.event, |movement| {
                movement.kind() == MovementKind::Input
                    && movement.asset() == &deposit.asset
                    && movement.from() == Some(&deposit.address)
            })?;
            if debit != allocation.gross_debit
                || allocation.asset != collection.asset
                || allocation.allocated_fee_asset != collection.asset
            {
                return Err(invalid(
                    "UTXO collection input differs from its durable allocation",
                ));
            }
            deposits.insert(
                deposit.id.clone(),
                (
                    deposit,
                    Movements {
                        outgoing,
                        input: true,
                        class: Some(Class::Collection),
                        ..Movements::default()
                    },
                ),
            );
        }
        let master_credit =
            leg.allocations
                .iter()
                .try_fold(base::Decimal::zero(), |total, allocation| {
                    total
                        .checked_add(&allocation.master_credit)
                        .map_err(|error| invalid(format!("collection amount overflowed: {error}")))
                })?;
        let output_credit = sum(&observation.event, |movement| {
            movement.kind() == MovementKind::Output
                && movement.asset() == &collection.asset
                && movement.to() == Some(&collection.destination)
        })?;
        let allocated_fee =
            leg.allocations
                .iter()
                .try_fold(base::Decimal::zero(), |total, allocation| {
                    total
                        .checked_add(&allocation.allocated_fee)
                        .map_err(|error| invalid(format!("collection amount overflowed: {error}")))
                })?;
        if output_credit != master_credit
            || observation
                .event
                .transaction
                .fee
                .as_ref()
                .is_none_or(|fee| fee.asset != collection.asset || fee.amount != allocated_fee)
        {
            return Err(invalid(
                "UTXO collection outputs or network fee differ from durable allocations",
            ));
        }
        let transition = match (&transaction.status, &leg.state) {
            (indexing::TransactionStatus::Included { .. }, CollectionLegState::Reorged { .. }) => {
                Some(UtxoBatchProjectionTransition::Reincluded {
                    included_at: transition_at,
                })
            }
            (
                indexing::TransactionStatus::Confirmed { .. },
                CollectionLegState::Broadcast { .. },
            )
            | (indexing::TransactionStatus::Confirmed { .. }, CollectionLegState::Reorged { .. }) => {
                Some(UtxoBatchProjectionTransition::Confirmed {
                    allocations: leg.allocations.clone(),
                    confirmed_at: transition_at,
                })
            }
            (indexing::TransactionStatus::Reorged { .. }, CollectionLegState::Confirmed { .. }) => {
                Some(UtxoBatchProjectionTransition::Reorged {
                    error: collection_error(
                        "ix_reorged",
                        "Indexer corrected a confirmed collection",
                    ),
                    reorged_at: transition_at,
                })
            }
            _ => None,
        };
        let mutation = transition.map(|transition| BatchMutation {
            collection_id: collection.id.clone(),
            leg_id: leg.id.clone(),
            expected: guard(collection, leg),
            transaction_id: transaction.transaction_id.clone(),
            transition,
        });
        return Ok((deposits, mutation));
    }

    let deposit = store
        .deposit(collection.deposit_id())
        .await?
        .ok_or_else(|| invalid("collection deposit is missing"))?;
    let class = match leg.kind {
        CollectionLegKind::Sweep => Class::Collection,
        CollectionLegKind::GasFunding => Class::GasFunding,
    };
    let outgoing = transaction
        .movements
        .iter()
        .filter(|movement| {
            movement.asset() == &deposit.asset && movement.from() == Some(&deposit.address)
        })
        .map(|movement| movement.id().clone())
        .collect();
    let incoming = transaction
        .movements
        .iter()
        .filter(|movement| {
            movement.asset() == &deposit.asset && movement.to() == Some(&deposit.address)
        })
        .map(|movement| movement.id().clone())
        .collect();
    advance_leg(store, observation, collection, leg, &deposit).await?;
    deposits.insert(
        deposit.id.clone(),
        (
            deposit,
            Movements {
                incoming,
                outgoing,
                input: false,
                class: Some(class),
            },
        ),
    );
    Ok((deposits, None))
}

async fn advance_leg(
    store: &dyn ObservationStore,
    observation: &MirroredObservation,
    collection: &Collection,
    leg: &CollectionLeg,
    deposit: &Deposit,
) -> Result<(), DepositError> {
    use indexing::TransactionStatus;
    let transaction_id = observation.event.transaction.transaction_id.clone();
    let at = collection.updated_at.max(observation.received_at);
    match (&observation.event.transaction.status, &leg.state) {
        (TransactionStatus::Confirmed { .. }, CollectionLegState::Broadcast { .. }) => {
            let allocation = if leg.kind == CollectionLegKind::Sweep {
                let allocation = super::allocation::collection(observation, collection, deposit)?;
                if collection.mode != CollectionMode::UtxoBatch {
                    let limit = leg
                        .planned_amount
                        .as_ref()
                        .ok_or_else(|| invalid("account-model sweep has no durable fee limit"))?;
                    if allocation.allocated_fee > *limit {
                        return Err(invalid(
                            "factual account-model fee exceeds its signed fee limit",
                        ));
                    }
                }
                Some(allocation)
            } else {
                super::gas::validate(observation, collection, leg, deposit)?;
                None
            };
            store
                .confirm_leg(deposits::ConfirmLeg {
                    collection_id: collection.id.clone(),
                    leg_id: leg.id.clone(),
                    expected: guard(collection, leg),
                    transaction_id,
                    allocation,
                    confirmed_at: at,
                })
                .await?;
        }
        (TransactionStatus::Reorged { .. }, CollectionLegState::Confirmed { .. }) => {
            let changed = store
                .reorg_leg(deposits::ReorgLeg {
                    collection_id: collection.id.clone(),
                    leg_id: leg.id.clone(),
                    expected: guard(collection, leg),
                    transaction_id,
                    error: collection_error(
                        "ix_reorged",
                        "Indexer corrected a confirmed collection",
                    ),
                    reorged_at: at,
                })
                .await?;
            release(store, changed, ReservationReleaseReason::Reorg, at).await?;
        }
        (
            TransactionStatus::Failed { .. }
            | TransactionStatus::Dropped
            | TransactionStatus::Replaced { .. },
            CollectionLegState::Broadcast { .. },
        ) => {
            let changed = store
                .fail_leg(deposits::FailLeg {
                    collection_id: collection.id.clone(),
                    leg_id: leg.id.clone(),
                    expected: guard(collection, leg),
                    transaction_id,
                    error: collection_error(
                        "ix_failed",
                        "Indexer reported a terminal collection transaction",
                    ),
                    failed_at: at,
                })
                .await?;
            release(
                store,
                changed,
                ReservationReleaseReason::TerminalFailure,
                at,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn sum(
    event: &indexing::ObservationEvent,
    mut predicate: impl FnMut(&ValueMovement) -> bool,
) -> Result<base::Decimal, DepositError> {
    event
        .transaction
        .movements
        .iter()
        .filter(|movement| predicate(movement))
        .try_fold(base::Decimal::zero(), |total, movement| {
            total
                .checked_add(movement.amount())
                .map_err(|error| invalid(format!("collection amount overflowed: {error}")))
        })
}
