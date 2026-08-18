use indexing::IndexScope;

use crate::{BoxFuture, DepositError};

pub const PAYMENT_SERVICE_OWNER: &str = "payment-service";
/// Version 4 binds the durable principal-scope model used for ownership and
/// idempotency.
pub const PAYMENT_DOMAIN_SCHEMA_VERSION: u16 = 4;

/// Durable authorization-independent model for principal ownership and
/// idempotency scopes. HTTP authentication types must not cross this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalScopeMode {
    RoleScoped,
    GlobalTrusted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyIdentity {
    pub version: String,
    pub digest: [u8; 32],
}

/// Path-global PS identity. One database is permanently bound to one service,
/// domain schema, chain scope, and active policy identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseIdentity {
    pub service_owner: String,
    pub domain_schema_version: u16,
    pub scope: IndexScope,
    pub active_policy: PolicyIdentity,
    pub principal_scope_mode: PrincipalScopeMode,
    pub initialized_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializeDatabase {
    pub scope: IndexScope,
    pub active_policy: PolicyIdentity,
    pub initialized_at: u64,
}

pub trait DatabaseInitializer: Send + Sync {
    /// Initializes an empty database or validates the immutable identity
    /// already present. Existing unbound PS data is rejected;
    /// an IX-owned database is always rejected.
    fn initialize_or_validate<'a>(
        &'a self,
        command: InitializeDatabase,
    ) -> BoxFuture<'a, Result<DatabaseIdentity, DepositError>> {
        self.initialize_or_validate_principal_scope(command, PrincipalScopeMode::RoleScoped)
    }

    /// Initializes or validates a database against the explicitly selected
    /// durable principal-scope model.
    fn initialize_or_validate_principal_scope<'a>(
        &'a self,
        command: InitializeDatabase,
        principal_scope_mode: PrincipalScopeMode,
    ) -> BoxFuture<'a, Result<DatabaseIdentity, DepositError>>;
}

pub trait MetadataReader: Send + Sync {
    fn database_metadata(&self) -> BoxFuture<'_, Result<Option<DatabaseIdentity>, DepositError>>;
}

pub trait DatabaseMetadata: DatabaseInitializer + MetadataReader {}

impl<T> DatabaseMetadata for T where T: DatabaseInitializer + MetadataReader {}
