use super::*;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct UserRecord {
    version: u16,
    id: String,
    owner: String,
    first_seen_at: u64,
}

#[derive(Clone, Copy, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum ModeRecord {
    RoleScoped,
    GlobalTrusted,
}

impl From<PrincipalScopeMode> for ModeRecord {
    fn from(value: PrincipalScopeMode) -> Self {
        match value {
            PrincipalScopeMode::RoleScoped => Self::RoleScoped,
            PrincipalScopeMode::GlobalTrusted => Self::GlobalTrusted,
        }
    }
}

impl From<ModeRecord> for PrincipalScopeMode {
    fn from(value: ModeRecord) -> Self {
        match value {
            ModeRecord::RoleScoped => Self::RoleScoped,
            ModeRecord::GlobalTrusted => Self::GlobalTrusted,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct DatabaseRecord {
    record_version: u16,
    service_owner: String,
    domain_schema_version: u16,
    scope: ScopeRecord,
    active_policy_version: String,
    active_policy_digest: [u8; 32],
    initialized_at: u64,
    principal_scope_mode: ModeRecord,
}

impl From<&DatabaseIdentity> for DatabaseRecord {
    fn from(value: &DatabaseIdentity) -> Self {
        Self {
            record_version: RECORD_VERSION,
            service_owner: value.service_owner.clone(),
            domain_schema_version: value.domain_schema_version,
            scope: ScopeRecord::from(&value.scope),
            active_policy_version: value.active_policy.version.clone(),
            active_policy_digest: value.active_policy.digest,
            initialized_at: value.initialized_at,
            principal_scope_mode: value.principal_scope_mode.into(),
        }
    }
}

impl TryFrom<DatabaseRecord> for DatabaseIdentity {
    type Error = DepositError;

    fn try_from(value: DatabaseRecord) -> Result<Self, Self::Error> {
        if value.record_version != RECORD_VERSION {
            return Err(storage_error(format!(
                "unsupported PS database metadata record version {}",
                value.record_version
            )));
        }
        Ok(Self {
            service_owner: value.service_owner,
            domain_schema_version: value.domain_schema_version,
            scope: value.scope.into(),
            active_policy: PolicyIdentity {
                version: value.active_policy_version,
                digest: value.active_policy_digest,
            },
            principal_scope_mode: value.principal_scope_mode.into(),
            initialized_at: value.initialized_at,
        })
    }
}

impl From<&User> for UserRecord {
    fn from(value: &User) -> Self {
        Self {
            version: RECORD_VERSION,
            id: value.id.0.clone(),
            owner: value.owner.0.clone(),
            first_seen_at: value.first_seen_at,
        }
    }
}

impl TryFrom<UserRecord> for User {
    type Error = DepositError;

    fn try_from(value: UserRecord) -> Result<Self, Self::Error> {
        ensure_version(value.version)?;
        Ok(Self {
            id: UserId(value.id),
            owner: CommandPrincipal(value.owner),
            first_seen_at: value.first_seen_at,
        })
    }
}

#[derive(Clone, Copy, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum OperationRecord {
    DepositPlan,
    CloseDeposit,
    CollectionPlan,
    RetryCollection,
    Accounting,
    ResolveReconciliation,
}

impl From<CommandOperation> for OperationRecord {
    fn from(value: CommandOperation) -> Self {
        match value {
            CommandOperation::DepositPlan => Self::DepositPlan,
            CommandOperation::CloseDeposit => Self::CloseDeposit,
            CommandOperation::CollectionPlan => Self::CollectionPlan,
            CommandOperation::RetryCollection => Self::RetryCollection,
            CommandOperation::Accounting => Self::Accounting,
            CommandOperation::ResolveReconciliation => Self::ResolveReconciliation,
        }
    }
}

impl From<OperationRecord> for CommandOperation {
    fn from(value: OperationRecord) -> Self {
        match value {
            OperationRecord::DepositPlan => Self::DepositPlan,
            OperationRecord::CloseDeposit => Self::CloseDeposit,
            OperationRecord::CollectionPlan => Self::CollectionPlan,
            OperationRecord::RetryCollection => Self::RetryCollection,
            OperationRecord::Accounting => Self::Accounting,
            OperationRecord::ResolveReconciliation => Self::ResolveReconciliation,
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct CommandRecord {
    principal: String,
    operation: OperationRecord,
    client_key: String,
    request_hash: [u8; 32],
}

impl From<&CommandIdentity> for CommandRecord {
    fn from(value: &CommandIdentity) -> Self {
        Self {
            principal: value.principal.0.clone(),
            operation: value.operation.into(),
            client_key: value.client_key.0.clone(),
            request_hash: value.request_hash.0,
        }
    }
}

impl From<CommandRecord> for CommandIdentity {
    fn from(value: CommandRecord) -> Self {
        Self {
            principal: CommandPrincipal(value.principal),
            operation: value.operation.into(),
            client_key: IdempotencyKey(value.client_key),
            request_hash: RequestHash(value.request_hash),
        }
    }
}
