use super::*;

fn resolved_movement_amounts(
    event: &ObservationEvent,
    deposit: &Deposit,
    movement_ids: &[MovementId],
) -> Result<Vec<Decimal>, DepositError> {
    let mut seen = std::collections::BTreeSet::new();
    movement_ids
        .iter()
        .map(|movement_id| {
            if !seen.insert(movement_id.clone()) {
                return Err(invalid(
                    "observation ledger effect contains a duplicate movement ID",
                ));
            }
            let mut matches = event
                .transaction
                .movements
                .iter()
                .filter(|movement| movement.id() == movement_id);
            let movement = matches.next().ok_or_else(|| {
                invalid("observation ledger effect references a missing IX movement")
            })?;
            if matches.next().is_some() {
                return Err(invalid(
                    "mirrored IX event contains a duplicate movement ID",
                ));
            }
            if movement.asset() != &deposit.asset {
                return Err(invalid(
                    "observation ledger movement asset does not match the deposit asset",
                ));
            }
            Ok(movement.amount().clone())
        })
        .collect()
}

fn resolved_net_balance_change(
    event: &ObservationEvent,
    deposit: &Deposit,
    debit_movements: &[MovementId],
    credit_movements: &[MovementId],
) -> Result<(Vec<Decimal>, Vec<Decimal>), DepositError> {
    if debit_movements.is_empty() || credit_movements.is_empty() {
        return Err(invalid(
            "net balance change requires both debit and credit movements",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut resolve = |movement_id: &MovementId,
                       expected_kind: MovementKind,
                       expected_address: &CanonicalAddress,
                       debit: bool|
     -> Result<Decimal, DepositError> {
        if !seen.insert(movement_id.clone()) {
            return Err(invalid(
                "net balance change contains a duplicate movement ID",
            ));
        }
        let mut matches = event
            .transaction
            .movements
            .iter()
            .filter(|movement| movement.id() == movement_id);
        let movement = matches
            .next()
            .ok_or_else(|| invalid("net balance change references a missing IX movement"))?;
        if matches.next().is_some() {
            return Err(invalid(
                "mirrored IX event contains a duplicate movement ID",
            ));
        }
        if movement.asset() != &deposit.asset
            || movement.kind() != expected_kind
            || if debit {
                movement.from() != Some(expected_address)
            } else {
                movement.to() != Some(expected_address)
            }
        {
            return Err(invalid(
                "net balance change movement does not match the deposit direction, asset, or address",
            ));
        }
        Ok(movement.amount().clone())
    };
    let debits = debit_movements
        .iter()
        .map(|movement_id| resolve(movement_id, MovementKind::Input, &deposit.address, true))
        .collect::<Result<Vec<_>, _>>()?;
    let credits = credit_movements
        .iter()
        .map(|movement_id| resolve(movement_id, MovementKind::Output, &deposit.address, false))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((debits, credits))
}

pub(super) fn resolved_network_fee(event: &ObservationEvent, deposit: &Deposit) -> Option<Decimal> {
    event
        .transaction
        .fee
        .as_ref()
        .filter(|fee| fee.asset == deposit.asset && fee.payer.as_ref() == Some(&deposit.address))
        .map(|fee| fee.amount.clone())
}

fn validate_input_debit_movements(
    event: &ObservationEvent,
    deposit: &Deposit,
    movement_ids: &[MovementId],
) -> Result<(), DepositError> {
    if movement_ids.is_empty() {
        return Err(invalid(
            "input-derived fee treatment requires at least one debit movement",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for movement_id in movement_ids {
        if !seen.insert(movement_id) {
            return Err(invalid(
                "input-derived fee treatment contains a duplicate movement ID",
            ));
        }
        let mut matches = event
            .transaction
            .movements
            .iter()
            .filter(|movement| movement.id() == movement_id);
        let movement = matches.next().ok_or_else(|| {
            invalid("input-derived fee treatment references a missing IX movement")
        })?;
        if matches.next().is_some()
            || movement.asset() != &deposit.asset
            || movement.kind() != MovementKind::Input
            || movement.from() != Some(&deposit.address)
        {
            return Err(invalid(
                "input-derived fee treatment requires factual inputs from the projected deposit",
            ));
        }
    }
    Ok(())
}

pub(super) fn projection_network_fee(
    event: &ObservationEvent,
    deposit: &Deposit,
    effect: &ObservationLedgerEffect,
    treatment: ProjectionFeeTreatment,
) -> Result<Option<Decimal>, DepositError> {
    match treatment {
        ProjectionFeeTreatment::Separate => Ok(resolved_network_fee(event, deposit)),
        ProjectionFeeTreatment::IncludedInMovementEffect => {
            match effect {
                LedgerEffect::Collection { movements } => {
                    validate_input_debit_movements(event, deposit, movements)?;
                }
                LedgerEffect::NetBalanceChange {
                    debit_movements, ..
                } => {
                    validate_input_debit_movements(event, deposit, debit_movements)?;
                }
                _ if resolved_network_fee(event, deposit).is_some() => {
                    return Err(invalid(
                        "fee-paying deposit requires an input-derived effect when the fee is included in movements",
                    ));
                }
                _ => return Ok(None),
            }
            event
                .transaction
                .fee
                .as_ref()
                .filter(|fee| fee.asset == deposit.asset)
                .ok_or_else(|| {
                    invalid(
                        "input-derived fee treatment requires a factual fee in the deposit asset",
                    )
                })?;
            Ok(None)
        }
    }
}

pub(super) fn resolved_effect(
    event: &ObservationEvent,
    deposit: &Deposit,
    effect: &ObservationLedgerEffect,
) -> Result<LedgerEffect<Decimal>, DepositError> {
    Ok(match effect {
        LedgerEffect::Incoming { movements } => LedgerEffect::Incoming {
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
        LedgerEffect::Collection { movements } => LedgerEffect::Collection {
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
        LedgerEffect::GasFunding { movements } => LedgerEffect::GasFunding {
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
        LedgerEffect::OtherBalanceChange {
            direction,
            movements,
        } => LedgerEffect::OtherBalanceChange {
            direction: *direction,
            movements: resolved_movement_amounts(event, deposit, movements)?,
        },
        LedgerEffect::NetBalanceChange {
            debit_movements,
            credit_movements,
        } => {
            let (debits, credits) =
                resolved_net_balance_change(event, deposit, debit_movements, credit_movements)?;
            LedgerEffect::NetBalanceChange {
                debit_movements: debits,
                credit_movements: credits,
            }
        }
    })
}

pub(super) fn projection_entry(
    command: &RecordObservation,
    event: &ObservationEvent,
    current: &LedgerEntry,
    effect: LedgerEffect<Decimal>,
    network_fee: Option<Decimal>,
) -> Result<LedgerEntry, DepositError> {
    let projection_id =
        ProjectionId::for_observation(&event.id, event.transaction.revision, &command.deposit_id);
    let balances = apply_observation_transition(
        current.balances.clone(),
        &LedgerTransition {
            status: event.transaction.status.clone(),
            previous_status: event.previous_status.clone(),
            effect,
            network_fee: network_fee.clone(),
        },
    )
    .map_err(|error| invalid(format!("invalid observation ledger transition: {error}")))?;
    Ok(LedgerEntry {
        id: EntryId(format!("projection:{}", projection_id.0)),
        deposit_id: command.deposit_id.clone(),
        previous: Some(current.id.clone()),
        cause: LedgerEntryCause::Observation {
            projection_id,
            event_id: event.id.clone(),
            observation_revision: event.transaction.revision,
            status: event.transaction.status.clone(),
            kind: command.effect.kind(),
            movement_ids: command
                .effect
                .movement_references()
                .into_iter()
                .cloned()
                .collect(),
            network_fee,
        },
        balances,
        recorded_at: command.recorded_at,
    })
}

impl<S> LedgerReader for PaymentStore<S>
where
    S: Store,
{
    fn current<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<LedgerEntry>, DepositError>> {
        Box::pin(async move { Ok(self.stored_head(deposit_id).await?.map(|(entry, _)| entry)) })
    }

    fn entries<'a>(
        &'a self,
        request: LedgerQuery,
    ) -> BoxFuture<'a, Result<LedgerPage, DepositError>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 1_000 {
                return Err(invalid("ledger page size must be between 1 and 1000"));
            }
            let page = self
                .storage
                .scan(ScanRequest {
                    namespace: ledger_entry_ns(),
                    prefix: ledger_prefix(&request.deposit_id)?,
                    after: request
                        .after
                        .as_ref()
                        .map(|entry| ledger_entry_key(&request.deposit_id, entry))
                        .transpose()?,
                    limit: request.limit,
                })
                .await
                .map_err(map_storage)?;
            let entries = page
                .entries
                .into_iter()
                .map(|(_, stored)| decode::<LedgerRecord>(&stored)?.try_into())
                .collect::<Result<Vec<LedgerEntry>, DepositError>>()?;
            let next = if page.next.is_some() {
                entries.last().map(|entry| entry.id.clone())
            } else {
                None
            };
            Ok(LedgerPage { entries, next })
        })
    }
}

impl<S> PaymentStore<S>
where
    S: Store,
{
    pub(super) async fn append_mirror_only(
        &self,
        observation: &MirroredObservation,
    ) -> Result<AppendOutcome, DepositError> {
        if let Some((existing, _)) = self.mirrored_observation(&observation.event.id).await? {
            return if existing == *observation {
                Ok(AppendOutcome::AlreadyPresent)
            } else {
                Err(conflict(
                    "IX event ID was reused with a different mirrored payload",
                ))
            };
        }
        let event_key = key_text(&observation.event.id.0);
        let cursor = cursor_key(observation.event.cursor);
        if let Some(stored) = self
            .storage
            .get(&observation_cursor_ns(), &cursor)
            .await
            .map_err(map_storage)?
        {
            let existing: IdRecord = decode(&stored)?;
            ensure_version(existing.version)?;
            return Err(conflict(format!(
                "IX cursor {} is already assigned to event {}",
                observation.event.cursor.0, existing.id
            )));
        }
        self.storage
            .commit(WriteBatch {
                conditions: vec![
                    Condition::Missing {
                        namespace: observation_ns(),
                        key: event_key.clone(),
                    },
                    Condition::Missing {
                        namespace: observation_cursor_ns(),
                        key: cursor.clone(),
                    },
                ],
                operations: vec![
                    Operation::Put {
                        namespace: observation_ns(),
                        key: event_key,
                        value: encode(&ObservationRecord::from(observation))?,
                    },
                    Operation::Put {
                        namespace: observation_cursor_ns(),
                        key: cursor,
                        value: encode(&IdRecord {
                            version: RECORD_VERSION,
                            id: observation.event.id.0.clone(),
                        })?,
                    },
                ],
            })
            .await
            .map_err(map_storage)?;
        Ok(AppendOutcome::Appended)
    }
}
