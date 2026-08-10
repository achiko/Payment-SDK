use indexing::IndexScope;

use crate::{BoxFuture, DepositError};

pub const PAYMENT_SERVICE_OWNER: &str = "payment-service";
/// Version 3 adds multi-participant collection records plus exact active
/// spend-resource ownership indexes. Older bound stores require explicit
/// semantic migration before the normal runtime may open them.
pub const PAYMENT_DOMAIN_SCHEMA_VERSION: u16 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyIdentity {
    pub version: String,
    pub digest: [u8; 32],
}

/// Path-global PS identity. One database is permanently bound to one service,
/// domain schema, chain scope, and active policy identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentDatabaseMetadata {
    pub service_owner: String,
    pub domain_schema_version: u16,
    pub scope: IndexScope,
    pub active_policy: PolicyIdentity,
    pub initialized_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializePaymentDatabase {
    pub scope: IndexScope,
    pub active_policy: PolicyIdentity,
    pub initialized_at: u64,
}

/// Explicit semantic migration performed only after the application has made
/// and verified the required physical RocksDB backup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigratePaymentDatabase {
    /// Operator-supplied scope used to bind legacy records that did not retain
    /// network identity. The concrete Ethereum-only runtime validates this
    /// assertion before invoking the repository.
    pub scope: IndexScope,
    pub active_policy: PolicyIdentity,
    pub migrated_at: u64,
    /// Bounds each validation and index-rebuild scan.
    pub page_size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentDatabaseMigrationReport {
    pub metadata: PaymentDatabaseMetadata,
    pub previous_domain_schema_version: Option<u16>,
    pub deposits: usize,
    pub ledger_entries: usize,
    pub mirrored_observations: usize,
    pub deposit_observations: usize,
    pub reconciliation_cases: usize,
    pub users: usize,
    pub jobs: usize,
    pub collections: usize,
    pub deposit_indexes_rebuilt: usize,
}

pub trait PaymentDatabaseMetadataStore: Send + Sync {
    /// Initializes an empty database or validates the immutable identity
    /// already present. Existing unbound PS data requires explicit migration;
    /// an IX-owned database is always rejected.
    fn initialize_or_validate<'a>(
        &'a self,
        command: InitializePaymentDatabase,
    ) -> BoxFuture<'a, Result<PaymentDatabaseMetadata, DepositError>>;

    fn database_metadata(
        &self,
    ) -> BoxFuture<'_, Result<Option<PaymentDatabaseMetadata>, DepositError>>;

    /// Validates and upgrades existing PS semantic records before binding the
    /// database to the current owner/schema/scope/policy. Implementations must
    /// reject IX-owned or mixed stores and must not write metadata until every
    /// validation and required supplementary-index rebuild succeeds.
    fn migrate_and_bind<'a>(
        &'a self,
        command: MigratePaymentDatabase,
    ) -> BoxFuture<'a, Result<PaymentDatabaseMigrationReport, DepositError>>;
}
