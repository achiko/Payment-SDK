use super::*;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum ReconciliationReasonRecord {
    PostCreditReorg {
        accounted: [u8; 32],
        corrected_confirmed: [u8; 32],
    },
    ReservedSpendConflict {
        collection_id: String,
        transaction_chain: String,
        transaction_network: String,
        transaction_id: String,
    },
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ReconciliationIdentity {
    pub(super) principal: String,
    pub(super) operation: u8,
    pub(super) client_key: String,
    pub(super) request_hash: [u8; 32],
}

impl From<&CommandIdentity> for ReconciliationIdentity {
    fn from(value: &CommandIdentity) -> Self {
        Self {
            principal: value.principal.0.clone(),
            operation: 0,
            client_key: value.client_key.0.clone(),
            request_hash: value.request_hash.0,
        }
    }
}

impl TryFrom<ReconciliationIdentity> for CommandIdentity {
    type Error = DepositError;

    fn try_from(value: ReconciliationIdentity) -> Result<Self, Self::Error> {
        if value.operation != 0 {
            return Err(storage_error(
                "reconciliation resolution record has an unknown command operation",
            ));
        }
        Ok(Self {
            principal: CommandPrincipal(value.principal),
            operation: CommandOperation::ResolveReconciliation,
            client_key: IdempotencyKey(value.client_key),
            request_hash: RequestHash(value.request_hash),
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum ReconciliationDecisionRecord {
    ReverseCredit {
        expected_head: String,
        reason: String,
    },
    AcceptLiability {
        reason: String,
    },
    ExternalDebtRecorded {
        external_reference: String,
        reason: String,
    },
}

impl From<&ReconciliationDecision> for ReconciliationDecisionRecord {
    fn from(value: &ReconciliationDecision) -> Self {
        match value {
            ReconciliationDecision::ReverseCredit {
                expected_head,
                reason,
            } => Self::ReverseCredit {
                expected_head: expected_head.0.clone(),
                reason: reason.clone(),
            },
            ReconciliationDecision::AcceptLiability { reason } => Self::AcceptLiability {
                reason: reason.clone(),
            },
            ReconciliationDecision::ExternalDebtRecorded {
                external_reference,
                reason,
            } => Self::ExternalDebtRecorded {
                external_reference: external_reference.clone(),
                reason: reason.clone(),
            },
        }
    }
}

impl From<ReconciliationDecisionRecord> for ReconciliationDecision {
    fn from(value: ReconciliationDecisionRecord) -> Self {
        match value {
            ReconciliationDecisionRecord::ReverseCredit {
                expected_head,
                reason,
            } => Self::ReverseCredit {
                expected_head: EntryId(expected_head),
                reason,
            },
            ReconciliationDecisionRecord::AcceptLiability { reason } => {
                Self::AcceptLiability { reason }
            }
            ReconciliationDecisionRecord::ExternalDebtRecorded {
                external_reference,
                reason,
            } => Self::ExternalDebtRecorded {
                external_reference,
                reason,
            },
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ResolutionRecord {
    pub(super) command: ReconciliationIdentity,
    pub(super) decision: ReconciliationDecisionRecord,
    pub(super) ledger_entry_id: Option<String>,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum ReconciliationStateRecord {
    Open,
    Resolved {
        resolution: ResolutionRecord,
        resolved_at: u64,
    },
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ReconciliationRecord {
    pub(super) version: u16,
    pub(super) id: String,
    pub(super) deposit_id: String,
    pub(super) triggering_event_id: String,
    pub(super) reason: ReconciliationReasonRecord,
    pub(super) state: ReconciliationStateRecord,
    pub(super) created_at: u64,
}

impl TryFrom<&ReconciliationCase> for ReconciliationRecord {
    type Error = DepositError;

    fn try_from(value: &ReconciliationCase) -> Result<Self, Self::Error> {
        let reason = match &value.reason {
            ReconciliationReason::PostCreditReorg {
                accounted,
                corrected_confirmed,
            } => ReconciliationReasonRecord::PostCreditReorg {
                accounted: amount::record_bytes(accounted),
                corrected_confirmed: amount::record_bytes(corrected_confirmed),
            },
            ReconciliationReason::ReservedSpendConflict {
                collection_id,
                transaction_id,
            } => ReconciliationReasonRecord::ReservedSpendConflict {
                collection_id: collection_id.0.clone(),
                transaction_chain: transaction_id.scope.chain.0.clone(),
                transaction_network: transaction_id.scope.network.clone(),
                transaction_id: transaction_id.value.clone(),
            },
        };
        let state = match &value.state {
            ReconciliationState::Open => ReconciliationStateRecord::Open,
            ReconciliationState::Resolved {
                resolution,
                resolved_at,
            } => ReconciliationStateRecord::Resolved {
                resolution: ResolutionRecord {
                    command: (&resolution.command).into(),
                    decision: (&resolution.decision).into(),
                    ledger_entry_id: resolution
                        .ledger_entry_id
                        .as_ref()
                        .map(|entry| entry.0.clone()),
                },
                resolved_at: *resolved_at,
            },
        };
        Ok(Self {
            version: RECONCILIATION_RECORD_VERSION,
            id: value.id.0.clone(),
            deposit_id: value.deposit_id.0.clone(),
            triggering_event_id: value.triggering_event_id.0.clone(),
            reason,
            state,
            created_at: value.created_at,
        })
    }
}

impl TryFrom<ReconciliationRecord> for ReconciliationCase {
    type Error = DepositError;

    fn try_from(value: ReconciliationRecord) -> Result<Self, Self::Error> {
        if value.version != RECONCILIATION_RECORD_VERSION {
            return Err(storage_error(format!(
                "unsupported PS reconciliation record version {}",
                value.version
            )));
        }
        Ok(Self {
            id: CaseId(value.id),
            deposit_id: DepositId(value.deposit_id),
            triggering_event_id: EventId(value.triggering_event_id),
            reason: match value.reason {
                ReconciliationReasonRecord::PostCreditReorg {
                    accounted,
                    corrected_confirmed,
                } => ReconciliationReason::PostCreditReorg {
                    accounted: amount::from_bytes(accounted),
                    corrected_confirmed: amount::from_bytes(corrected_confirmed),
                },
                ReconciliationReasonRecord::ReservedSpendConflict {
                    collection_id,
                    transaction_chain,
                    transaction_network,
                    transaction_id,
                } => ReconciliationReason::ReservedSpendConflict {
                    collection_id: crate::CollectionId(collection_id),
                    transaction_id: TransactionRef {
                        scope: IndexScope {
                            chain: ChainId(transaction_chain),
                            network: transaction_network,
                        },
                        value: transaction_id,
                    },
                },
            },
            state: match value.state {
                ReconciliationStateRecord::Open => ReconciliationState::Open,
                ReconciliationStateRecord::Resolved {
                    resolution,
                    resolved_at,
                } => {
                    let decision = ReconciliationDecision::from(resolution.decision);
                    let ledger_entry_id = resolution.ledger_entry_id.map(EntryId);
                    if matches!(decision, ReconciliationDecision::ReverseCredit { .. })
                        != ledger_entry_id.is_some()
                    {
                        return Err(storage_error(
                            "typed reconciliation ledger entry does not match its decision",
                        ));
                    }
                    ReconciliationState::Resolved {
                        resolution: ReconciliationResolution {
                            command: resolution.command.try_into()?,
                            decision,
                            ledger_entry_id,
                        },
                        resolved_at,
                    }
                }
            },
            created_at: value.created_at,
        })
    }
}
