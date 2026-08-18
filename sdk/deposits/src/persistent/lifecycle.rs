use super::deposit::transition_allowed;
use super::*;

impl<S> DepositLifecycle for PaymentStore<S>
where
    S: Store,
{
    fn set_state<'a>(
        &'a self,
        id: &'a DepositId,
        state: DepositState,
    ) -> BoxFuture<'a, Result<(), DepositError>> {
        Box::pin(async move {
            if state == DepositState::Closed {
                return Err(invalid_state(
                    "deposit closure must use the guarded close command",
                ));
            }
            let (mut deposit, stored) = self
                .stored_deposit(id)
                .await?
                .ok_or_else(|| not_found("deposit does not exist"))?;
            if !transition_allowed(&deposit.state, &state) {
                return Err(invalid_state("deposit lifecycle transition is not allowed"));
            }
            if deposit.state == state {
                return Ok(());
            }
            let previous_kind = deposit.state.kind();
            let next_kind = state.kind();
            let was_awaiting = deposit.state == DepositState::AwaitingWatch;
            let is_awaiting = state == DepositState::AwaitingWatch;
            deposit.state = state;
            let mut operations = vec![Operation::Put {
                namespace: deposit_ns(),
                key: key_text(&id.0),
                value: encode(&DepositRecord::from(&deposit))?,
            }];
            if was_awaiting && !is_awaiting {
                operations.push(Operation::Delete {
                    namespace: awaiting_watch_ns(),
                    key: key_text(&id.0),
                });
            }
            let mut conditions = vec![Condition::Version {
                namespace: deposit_ns(),
                key: key_text(&id.0),
                expected: stored.version,
            }];
            if previous_kind != next_kind {
                let next_state_key = state_deposit_key(next_kind, id)?;
                let next_user_state_key = user_state_deposit_key(&deposit.user_id, next_kind, id)?;
                conditions.extend([
                    Condition::Missing {
                        namespace: deposit_state_ns(),
                        key: next_state_key.clone(),
                    },
                    Condition::Missing {
                        namespace: user_deposit_state_ns(),
                        key: next_user_state_key.clone(),
                    },
                ]);
                let index = IdRecord {
                    version: RECORD_VERSION,
                    id: id.0.clone(),
                };
                operations.extend([
                    Operation::Delete {
                        namespace: deposit_state_ns(),
                        key: state_deposit_key(previous_kind, id)?,
                    },
                    Operation::Delete {
                        namespace: user_deposit_state_ns(),
                        key: user_state_deposit_key(&deposit.user_id, previous_kind, id)?,
                    },
                    Operation::Put {
                        namespace: deposit_state_ns(),
                        key: next_state_key,
                        value: encode(&index)?,
                    },
                    Operation::Put {
                        namespace: user_deposit_state_ns(),
                        key: next_user_state_key,
                        value: encode(&index)?,
                    },
                ]);
            }
            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)?;
            Ok(())
        })
    }

    fn close<'a>(&'a self, command: CloseDeposit) -> BoxFuture<'a, Result<(), DepositError>> {
        Box::pin(async move {
            let (mut deposit, deposit_stored) = self
                .stored_deposit(&command.deposit_id)
                .await?
                .ok_or_else(|| not_found("deposit does not exist"))?;
            if deposit.state == DepositState::Closed {
                return Ok(());
            }
            if deposit.state != command.expected_state {
                return Err(conflict("deposit state changed before close"));
            }
            let retained_watch = match &deposit.state {
                DepositState::Active { watch_id } | DepositState::Expired { watch_id } => {
                    watch_id.clone()
                }
                DepositState::AwaitingWatch | DepositState::Closed => {
                    return Err(invalid_state(
                        "only an observed active or expired deposit can close",
                    ));
                }
            };
            let (head, head_stored) = self
                .stored_head(&command.deposit_id)
                .await?
                .ok_or_else(|| storage_error("closing deposit has no ledger head"))?;
            if head.id != command.expected_ledger_head {
                return Err(conflict("deposit ledger head changed before close"));
            }
            if !head.balances.balance.is_zero() {
                return Err(invalid_state(
                    "deposit cannot close while its current balance is non-zero",
                ));
            }
            if self.automatic_actions_blocked(&command.deposit_id).await? {
                return Err(invalid_state(
                    "deposit cannot close while reconciliation is unresolved",
                ));
            }
            let (reconciliation_condition, reconciliation_operation) = self
                .reconciliation_generation_change(&command.deposit_id)
                .await?;

            let reservation_fence = self
                .prepare_deposit_close_reservation_fence(&command.deposit_id, &deposit.asset)
                .await?;
            let (collection_condition, collection_operation) = self
                .collection_eligibility_generation_change(&command.deposit_id, &deposit.asset)
                .await?;

            let previous_kind = deposit.state.kind();
            let next_kind = DepositStateKind::Closed;
            let closed_state_key = state_deposit_key(next_kind, &command.deposit_id)?;
            let closed_user_state_key =
                user_state_deposit_key(&deposit.user_id, next_kind, &command.deposit_id)?;
            let closed_watch_key = key_text(&command.deposit_id.0);
            let index = IdRecord {
                version: RECORD_VERSION,
                id: command.deposit_id.0.clone(),
            };
            deposit.state = DepositState::Closed;

            let mut conditions = vec![
                Condition::Version {
                    namespace: deposit_ns(),
                    key: key_text(&command.deposit_id.0),
                    expected: deposit_stored.version,
                },
                Condition::Version {
                    namespace: ledger_head_ns(),
                    key: key_text(&command.deposit_id.0),
                    expected: head_stored.version,
                },
                Condition::Missing {
                    namespace: closed_deposit_watch_ns(),
                    key: closed_watch_key.clone(),
                },
                Condition::Missing {
                    namespace: deposit_state_ns(),
                    key: closed_state_key.clone(),
                },
                Condition::Missing {
                    namespace: user_deposit_state_ns(),
                    key: closed_user_state_key.clone(),
                },
                reconciliation_condition,
                collection_condition,
            ];
            conditions.extend(reservation_fence.conditions);

            let mut operations = vec![
                Operation::Put {
                    namespace: deposit_ns(),
                    key: key_text(&command.deposit_id.0),
                    value: encode(&DepositRecord::from(&deposit))?,
                },
                // Rewriting the same head ID increments its storage
                // version, invalidating a projection that read the zero
                // head before this close committed.
                Operation::Put {
                    namespace: ledger_head_ns(),
                    key: key_text(&command.deposit_id.0),
                    value: encode(&IdRecord {
                        version: RECORD_VERSION,
                        id: head.id.0,
                    })?,
                },
                Operation::Delete {
                    namespace: deposit_state_ns(),
                    key: state_deposit_key(previous_kind, &command.deposit_id)?,
                },
                Operation::Delete {
                    namespace: user_deposit_state_ns(),
                    key: user_state_deposit_key(
                        &deposit.user_id,
                        previous_kind,
                        &command.deposit_id,
                    )?,
                },
                Operation::Put {
                    namespace: deposit_state_ns(),
                    key: closed_state_key,
                    value: encode(&index)?,
                },
                Operation::Put {
                    namespace: user_deposit_state_ns(),
                    key: closed_user_state_key,
                    value: encode(&index)?,
                },
                // Keep the durable watch relationship after closure.
                // Late transfers remain visible; a future explicit IX
                // cutoff protocol can use this retained identifier.
                Operation::Put {
                    namespace: closed_deposit_watch_ns(),
                    key: closed_watch_key,
                    value: encode(&IdRecord {
                        version: RECORD_VERSION,
                        id: retained_watch.0,
                    })?,
                },
                reconciliation_operation,
                collection_operation,
            ];
            operations.extend(reservation_fence.operations);

            self.storage
                .commit(WriteBatch {
                    conditions,
                    operations,
                })
                .await
                .map_err(map_storage)?;
            Ok(())
        })
    }
}
