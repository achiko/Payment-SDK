use super::*;

pub(super) fn map_storage(error: Error) -> DepositError {
    let kind = match error.kind {
        ErrorKind::Conflict => DepositErrorKind::Conflict,
        ErrorKind::CorruptData | ErrorKind::InvalidRequest => DepositErrorKind::InvariantViolation,
        ErrorKind::Unavailable | ErrorKind::Other => DepositErrorKind::Store,
    };
    DepositError {
        kind,
        message: error.message,
    }
}

pub(super) fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

pub(super) fn conflict(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Conflict,
        message: message.into(),
    }
}

pub(super) fn not_found(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::NotFound,
        message: message.into(),
    }
}

pub(super) fn invalid_state(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvalidState,
        message: message.into(),
    }
}

pub(super) fn storage_error(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Store,
        message: message.into(),
    }
}

pub(super) fn validate_non_empty(value: &str, name: &str) -> Result<(), DepositError> {
    if value.trim().is_empty() {
        Err(invalid(format!("{name} must not be empty")))
    } else {
        Ok(())
    }
}

impl JobPlan {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        let command = self;
        validate_non_empty(&command.id.0, "job ID")?;
        validate_non_empty(&command.command.principal.0, "command principal")?;
        validate_non_empty(&command.command.client_key.0, "command idempotency key")?;
        if let Some(user_id) = command.payload.user_id() {
            validate_non_empty(&user_id.0, "user ID")?;
        }
        validate_non_empty(&command.user_owner.0, "user owner principal")?;
        validate_non_empty(&command.policy.version, "job policy version")?;
        if command.command.operation != command.payload.operation() {
            return Err(invalid(
                "command operation does not match the durable job payload",
            ));
        }
        match &command.payload {
            JobPayload::DepositPlan(payload) => {
                validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
                validate_non_empty(&payload.scope.chain.0, "Indexer scope chain")?;
                validate_non_empty(&payload.scope.network, "Indexer scope network")?;
                validate_non_empty(&payload.asset.asset, "asset ID")?;
                validate_non_empty(&payload.key_purpose, "key purpose")?;
                if payload.asset.chain != payload.scope.chain {
                    return Err(invalid(
                        "deposit job asset and Indexer scope must share a chain",
                    ));
                }
                if payload.created_at != command.created_at {
                    return Err(invalid(
                        "deposit payload creation time must match the job creation time",
                    ));
                }
                if payload.expires_at < payload.created_at {
                    return Err(invalid("deposit job expiration precedes creation"));
                }
            }
            JobPayload::CloseDeposit(payload) => {
                validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
            }
            JobPayload::CollectionPlan(payload) => {
                validate_non_empty(&payload.collection_id.0, "collection ID")?;
                validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
            }
            JobPayload::RetryCollection(payload) => {
                validate_non_empty(&payload.collection_id.0, "collection ID")?;
                validate_non_empty(&payload.deposit_id.0, "deposit ID")?;
            }
            JobPayload::CreateBatch(payload) => {
                validate_non_empty(&payload.collection_id.0, "collection ID")?;
                validate_canonical_deposit_ids(&payload.deposit_ids)?;
            }
            JobPayload::RetryUtxoBatchCollection(payload) => {
                validate_non_empty(&payload.collection_id.0, "collection ID")?;
                validate_canonical_deposit_ids(&payload.deposit_ids)?;
            }
        }
        Ok(())
    }
}

pub(super) fn validate_canonical_deposit_ids(
    deposit_ids: &[DepositId],
) -> Result<(), DepositError> {
    if deposit_ids.is_empty() {
        return Err(invalid("UTXO-batch job must contain at least one deposit"));
    }
    let mut previous = None;
    for deposit_id in deposit_ids {
        validate_non_empty(&deposit_id.0, "UTXO-batch deposit ID")?;
        if previous
            .as_ref()
            .is_some_and(|current| current >= deposit_id)
        {
            return Err(invalid(
                "UTXO-batch job deposit IDs must be strictly canonical and unique",
            ));
        }
        previous = Some(deposit_id.clone());
    }
    Ok(())
}

impl JobQuery {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        if self.limit == 0 || self.limit > MAX_PAGE_SIZE {
            Err(invalid("job page size must be between 1 and 1000"))
        } else {
            Ok(())
        }
    }
}

impl JobError {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        validate_non_empty(&self.code, "job error code")?;
        validate_non_empty(&self.message, "job error message")
    }
}

impl InitializeDatabase {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        validate_non_empty(&self.scope.chain.0, "Payment Service scope chain")?;
        validate_non_empty(&self.scope.network, "Payment Service scope network")?;
        validate_non_empty(&self.active_policy.version, "active policy version")
    }
}

pub(super) fn expected_metadata(
    command: InitializeDatabase,
    principal_scope_mode: PrincipalScopeMode,
) -> DatabaseIdentity {
    DatabaseIdentity {
        service_owner: PAYMENT_SERVICE_OWNER.to_owned(),
        domain_schema_version: PAYMENT_DOMAIN_SCHEMA_VERSION,
        scope: command.scope,
        active_policy: command.active_policy,
        principal_scope_mode,
        initialized_at: command.initialized_at,
    }
}

pub(super) fn validate_persisted_metadata(
    persisted: DatabaseIdentity,
    expected: &DatabaseIdentity,
) -> Result<DatabaseIdentity, DepositError> {
    if persisted.service_owner != PAYMENT_SERVICE_OWNER {
        return Err(conflict(format!(
            "database is owned by {}, not Payment Service",
            persisted.service_owner
        )));
    }
    if persisted.domain_schema_version != PAYMENT_DOMAIN_SCHEMA_VERSION {
        return Err(conflict(format!(
            "Payment Service domain schema version {} does not match runtime version {}",
            persisted.domain_schema_version, PAYMENT_DOMAIN_SCHEMA_VERSION
        )));
    }
    if persisted.scope != expected.scope {
        return Err(conflict(
            "Payment Service database is bound to a different Indexer scope",
        ));
    }
    if persisted.active_policy != expected.active_policy {
        return Err(conflict(
            "Payment Service database is bound to a different active policy",
        ));
    }
    if persisted.principal_scope_mode != expected.principal_scope_mode {
        return Err(conflict(
            "Payment Service database is bound to a different principal-scope mode",
        ));
    }
    Ok(persisted)
}

impl Job {
    pub(super) fn ready_at(&self) -> Option<u64> {
        match &self.state {
            JobState::Queued => Some(self.created_at),
            JobState::Running { lease_expires_at } => Some(*lease_expires_at),
            JobState::WaitingRetry { next_attempt_at } => Some(*next_attempt_at),
            JobState::Succeeded | JobState::Failed => None,
        }
    }
}
