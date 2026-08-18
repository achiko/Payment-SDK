use super::*;

impl ReconciliationCase {
    pub(super) fn validate_open(&self) -> Result<(), DepositError> {
        let case = self;
        if case.id.0.is_empty()
            || case.deposit_id.0.is_empty()
            || case.triggering_event_id.0.is_empty()
        {
            return Err(invalid(
                "reconciliation case, deposit, and triggering event IDs must be non-empty",
            ));
        }
        if case.state != ReconciliationState::Open {
            return Err(invalid("a new reconciliation case must be open"));
        }
        match &case.reason {
            ReconciliationReason::PostCreditReorg {
                accounted,
                corrected_confirmed,
            } if accounted > corrected_confirmed => Ok(()),
            ReconciliationReason::PostCreditReorg { .. } => Err(invalid(
                "post-credit reorg requires accounted to exceed corrected confirmed",
            )),
            ReconciliationReason::ReservedSpendConflict {
                collection_id,
                transaction_id,
            } if !collection_id.0.is_empty()
                && !transaction_id.scope.chain.0.is_empty()
                && !transaction_id.scope.network.is_empty()
                && !transaction_id.value.is_empty() =>
            {
                Ok(())
            }
            ReconciliationReason::ReservedSpendConflict { .. } => Err(invalid(
                "reserved-spend conflict requires collection and transaction identity",
            )),
        }
    }
}

impl ReconciliationDecision {
    pub(super) fn reason(&self) -> &str {
        match self {
            ReconciliationDecision::ReverseCredit { reason, .. }
            | ReconciliationDecision::AcceptLiability { reason }
            | ReconciliationDecision::ExternalDebtRecorded { reason, .. } => reason,
        }
    }
}

impl ResolveReconciliation {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        let command = self;
        if command.command.operation != CommandOperation::ResolveReconciliation {
            return Err(invalid(
                "reconciliation command identity must use the resolve-reconciliation operation",
            ));
        }
        if command.command.principal.0.is_empty()
            || command.command.client_key.0.is_empty()
            || command.case_id.0.is_empty()
        {
            return Err(invalid(
                "reconciliation principal, client key, and case ID must be non-empty",
            ));
        }
        let reason = command.decision.reason();
        if reason.trim().is_empty() {
            return Err(invalid(
                "reconciliation resolution reason must not be blank",
            ));
        }
        if reason.len() > MAX_RECONCILIATION_REASON_BYTES {
            return Err(invalid(format!(
                "reconciliation resolution reason must not exceed {MAX_RECONCILIATION_REASON_BYTES} bytes"
            )));
        }
        match &command.decision {
            ReconciliationDecision::ReverseCredit { expected_head, .. }
                if expected_head.0.is_empty() =>
            {
                Err(invalid(
                    "reverse-credit resolution requires a non-empty expected ledger head",
                ))
            }
            ReconciliationDecision::ExternalDebtRecorded {
                external_reference, ..
            } if external_reference.trim().is_empty()
                || external_reference.len() > MAX_EXTERNAL_DEBT_REFERENCE_BYTES
                || external_reference
                    .bytes()
                    .any(|byte| byte.is_ascii_control()) =>
            {
                Err(invalid(format!(
                    "external debt reference must contain between 1 and {MAX_EXTERNAL_DEBT_REFERENCE_BYTES} safe bytes"
                )))
            }
            _ => Ok(()),
        }
    }
}

pub(super) fn reconciliation_resolution_entry(
    command: &ResolveReconciliation,
    current: &LedgerEntry,
) -> LedgerEntry {
    let mut balances = current.balances.clone();
    balances.accounted = balances.accounted.clone().min(balances.confirmed.clone());
    LedgerEntry {
        id: opaque_command_ledger_entry_id("reconciliation", &command.command),
        deposit_id: current.deposit_id.clone(),
        previous: Some(current.id.clone()),
        cause: LedgerEntryCause::ReconciliationResolution {
            case_id: command.case_id.clone(),
            idempotency_key: command.command.client_key.clone(),
            reason: command.decision.reason().to_owned(),
        },
        balances,
        recorded_at: command.resolved_at,
    }
}

pub(super) fn opaque_command_ledger_entry_id(kind: &str, command: &CommandIdentity) -> EntryId {
    let mut digest = Sha256::new();
    digest.update(b"payment-service-ledger-entry-v1");
    update_hash_component(&mut digest, kind.as_bytes());
    update_hash_component(&mut digest, command.principal.0.as_bytes());
    update_hash_component(&mut digest, command_operation_tag(command.operation));
    update_hash_component(&mut digest, command.client_key.0.as_bytes());
    update_hash_component(&mut digest, &command.request_hash.0);
    let digest = digest.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    EntryId(format!("{kind}:{encoded}"))
}

pub(super) fn update_hash_component(digest: &mut Sha256, component: &[u8]) {
    digest.update(
        u64::try_from(component.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(component);
}

// design-lint: allow unclassified-free-function -- hashes the stable command-operation wire tag
pub(super) const fn command_operation_tag(operation: CommandOperation) -> &'static [u8] {
    match operation {
        CommandOperation::DepositPlan => b"create_deposit",
        CommandOperation::CloseDeposit => b"close_deposit",
        CommandOperation::CollectionPlan => b"create_collection",
        CommandOperation::RetryCollection => b"retry_collection",
        CommandOperation::Accounting => b"accounting",
        CommandOperation::ResolveReconciliation => b"resolve_reconciliation",
    }
}
