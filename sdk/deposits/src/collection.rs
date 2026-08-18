use crate::{BoxFuture, DepositError, DepositId, EntryId, JobId, PolicyIdentity, UserId};
use base::Decimal;
use indexing::WatchId;
use indexing::{AssetId, CanonicalAddress, TransactionRef};
use std::fmt;

/// Maximum opaque signed transaction size accepted by PS persistence.
pub const MAX_SIGNED_ENVELOPE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum opaque evidence retained for one exact spend resource.
///
/// The evidence is PS-owned replay material (for example, the bounded UTXO
/// snapshot used for policy approval). It is not a replacement for the exact
/// outpoint identity and must never contain signing secrets.
pub const MAX_SPEND_RESOURCE_EVIDENCE_BYTES: usize = 64 * 1024;
pub const MAX_COLLECTION_PARTICIPANTS: usize = 4_096;
pub const MAX_COLLECTION_SPEND_RESOURCES: usize = 16_384;
pub const MAX_TOTAL_SPEND_RESOURCE_EVIDENCE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LegId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionMode {
    AccountTransfer,
    UtxoBatch,
    TokenWithGas,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionState {
    Required,
    InProgress,
    Completed,
    Failed,
    Reorged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionLegKind {
    GasFunding,
    Sweep,
}

/// Durable transaction state for one ordered collection leg.
///
/// A signed transaction ID is the hash computed from the opaque envelope
/// before broadcast. `Broadcast` is persisted before IX watch registration,
/// preserving the response-loss and watch-registration failure windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollectionLegState {
    Required,
    Signed { transaction_id: TransactionRef },
    Broadcast { transaction_id: TransactionRef },
    Confirmed { transaction_id: TransactionRef },
    Failed { transaction_id: TransactionRef },
    Reorged { transaction_id: TransactionRef },
}

impl CollectionLegState {
    #[must_use]
    pub const fn transaction_id(&self) -> Option<&TransactionRef> {
        match self {
            Self::Required => None,
            Self::Signed { transaction_id }
            | Self::Broadcast { transaction_id }
            | Self::Confirmed { transaction_id }
            | Self::Failed { transaction_id }
            | Self::Reorged { transaction_id } => Some(transaction_id),
        }
    }
}

/// Diagnostic failure safe to expose through an authenticated PS API.
/// Credentials, signed envelopes, RPC URLs, and custody details must never be
/// placed in this record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationReleaseReason {
    TerminalFailure,
    Reorg,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollectionReservationState {
    Active,
    Consumed {
        transaction_id: TransactionRef,
        consumed_at: u64,
    },
    Released {
        reason: ReservationReleaseReason,
        released_at: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionReservation {
    pub deposit_id: DepositId,
    pub asset: AssetId,
    pub amount: Decimal,
    pub state: CollectionReservationState,
}

/// Exact UTXO spend-resource identity. Output indexes are interpreted in the
/// transaction identified by `transaction_id`; string-only or address-level
/// reservations are deliberately insufficient for UTXO collection.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceId {
    pub transaction_id: TransactionRef,
    pub output_index: u32,
}

/// Bounded opaque evidence retained with an exact spend-resource reservation.
/// Its redacting `Debug` implementation reports only byte length so chain
/// evidence cannot enter ordinary diagnostic output.
#[derive(Clone, PartialEq, Eq)]
pub struct ResourceProof(Vec<u8>);

impl fmt::Debug for ResourceProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceProof")
            .field("byte_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl ResourceProof {
    /// Creates non-empty bounded replay evidence.
    ///
    /// # Errors
    ///
    /// Returns an invariant error for empty or oversized evidence.
    pub fn new(bytes: Vec<u8>) -> Result<Self, DepositError> {
        if bytes.is_empty() {
            return Err(invalid("spend-resource evidence must not be empty"));
        }
        if bytes.len() > MAX_SPEND_RESOURCE_EVIDENCE_BYTES {
            return Err(invalid(
                "spend-resource evidence exceeds the PS persistence limit",
            ));
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// One exact resource reserved for a collection participant.
///
/// `amount` is duplicated deliberately from the chain evidence as a stable,
/// integer PS policy input. Persistence validates that the participant's
/// reservation is exactly the checked sum of its resources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendResource {
    pub id: ResourceId,
    pub amount: Decimal,
    pub evidence: ResourceProof,
}

/// One deposit participating in a collection aggregate.
///
/// Account-model collections contain exactly one participant with no exact
/// spend resources. A UTXO batch contains one or more participants,
/// each with one or more canonically ordered outpoint reservations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionParticipant {
    pub user_id: UserId,
    pub reservation: CollectionReservation,
    pub spend_resources: Vec<SpendResource>,
}

/// Confirmed factual attribution for the sweep transaction.
///
/// `allocated_fee_asset` may differ from `asset` for token collection, where
/// the token debit and native network fee are deliberately separate values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionAllocation {
    pub deposit_id: DepositId,
    pub asset: AssetId,
    pub gross_debit: Decimal,
    pub master_credit: Decimal,
    pub allocated_fee_asset: AssetId,
    pub allocated_fee: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionLeg {
    pub id: LegId,
    /// Stable zero-based execution order within the aggregate.
    pub position: u16,
    pub kind: CollectionLegKind,
    /// PS-owned amount fixed before execution. For gas funding this is the
    /// transferred amount. For an account-model sweep this is the signed
    /// transaction's maximum native fee, not the factual receipt fee. UTXO
    /// sweep value is represented by the aggregate reservation and leaves this
    /// field empty.
    pub planned_amount: Option<Decimal>,
    pub state: CollectionLegState,
    /// Durable IX watch registration, retained after terminal state changes.
    pub watch_id: Option<WatchId>,
    pub attempt_count: u32,
    /// Compatibility projection for one-source account-model callers. It is
    /// `Some` exactly when `allocations` contains one item.
    pub allocation: Option<CollectionAllocation>,
    /// Factual per-deposit attribution. UTXO batches require one allocation
    /// for every participant in canonical participant order.
    pub allocations: Vec<CollectionAllocation>,
    pub last_error: Option<CollectionError>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: CollectionId,
    /// Stable association with the durable command job that created it.
    pub job_id: JobId,
    pub mode: CollectionMode,
    pub asset: AssetId,
    pub destination: CanonicalAddress,
    pub policy: PolicyIdentity,
    pub state: CollectionState,
    /// Canonically ordered participant reservations.
    pub participants: Vec<CollectionParticipant>,
    pub legs: Vec<CollectionLeg>,
    pub attempt_count: u32,
    pub last_error: Option<CollectionError>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Collection {
    #[must_use]
    pub fn primary(&self) -> &CollectionParticipant {
        self.participants
            .first()
            .expect("a validated collection always has a primary participant")
    }

    #[must_use]
    pub fn user_id(&self) -> &UserId {
        &self.primary().user_id
    }

    #[must_use]
    pub fn deposit_id(&self) -> &DepositId {
        &self.primary().reservation.deposit_id
    }

    #[must_use]
    pub fn reservation(&self) -> &CollectionReservation {
        &self.primary().reservation
    }

    #[must_use]
    pub fn participant(&self, deposit_id: &DepositId) -> Option<&CollectionParticipant> {
        self.participants
            .iter()
            .find(|participant| &participant.reservation.deposit_id == deposit_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateLeg {
    pub id: LegId,
    pub kind: CollectionLegKind,
    /// Required and positive for `GasFunding`; forbidden for `Sweep`.
    pub planned_amount: Option<Decimal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionPlan {
    pub id: CollectionId,
    pub job_id: JobId,
    pub user_id: UserId,
    pub deposit_id: DepositId,
    pub mode: CollectionMode,
    pub asset: AssetId,
    pub destination: CanonicalAddress,
    pub policy: PolicyIdentity,
    pub reservation_amount: Decimal,
    pub legs: Vec<CreateLeg>,
    pub created_at: u64,
}

/// One participant supplied when atomically creating a UTXO batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchParticipant {
    pub user_id: UserId,
    pub deposit_id: DepositId,
    /// Ledger head against which the caller selected and approved these exact
    /// spend resources. Creation atomically fences every participant head so
    /// an intervening IX projection cannot leave a stale UTXO reservation.
    pub expected_ledger_head: EntryId,
    pub reservation_amount: Decimal,
    pub spend_resources: Vec<SpendResource>,
}

/// Creates one UTXO collection aggregate over an exact, canonically
/// ordered set of participant outpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateBatch {
    pub id: CollectionId,
    pub job_id: JobId,
    pub asset: AssetId,
    pub destination: CanonicalAddress,
    pub policy: PolicyIdentity,
    pub participants: Vec<BatchParticipant>,
    pub leg: CreateLeg,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateCollectionOutcome {
    Created { collection: Collection },
    Replayed { collection: Collection },
}

impl CreateCollectionOutcome {
    #[must_use]
    pub const fn collection(&self) -> &Collection {
        match self {
            Self::Created { collection } | Self::Replayed { collection } => collection,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionGuard {
    pub collection_state: CollectionState,
    pub leg_state: CollectionLegState,
}

/// Opaque signed chain-native bytes. This type intentionally has no `Debug`
/// implementation so envelopes cannot enter ordinary diagnostic output.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedBytes(Vec<u8>);

impl SignedBytes {
    /// Creates a bounded non-empty opaque envelope.
    ///
    /// # Errors
    ///
    /// Returns an invariant error for an empty or oversized envelope.
    pub fn new(bytes: Vec<u8>) -> Result<Self, DepositError> {
        if bytes.is_empty() {
            return Err(invalid("signed envelope must not be empty"));
        }
        if bytes.len() > MAX_SIGNED_ENVELOPE_BYTES {
            return Err(invalid("signed envelope exceeds the PS persistence limit"));
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Durable recovery envelope persisted before any broadcast attempt.
/// Account-model envelopes are deleted when broadcast is accepted. UTXO-batch
/// envelopes are retained across accepted broadcast, confirmation, and reorg
/// so the same transaction can be monitored/rebroadcast without releasing
/// outpoints or signing different bytes. `expires_at` is only an operational
/// retention/alerting hint. This type intentionally has no `Debug`
/// implementation.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedEnvelope {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected_transaction_id: TransactionRef,
    pub bytes: SignedBytes,
    pub signed_at: u64,
    pub expires_at: u64,
}

/// Persists the signed envelope, transaction attribution index, and `Signed`
/// leg state in one atomic write.
/// This command intentionally has no `Debug` implementation.
pub struct RecordSignature {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub expected_transaction_id: TransactionRef,
    pub envelope: SignedBytes,
    /// Required, canonical, and one-per-participant for UTXO batches. Other
    /// modes leave this empty and attach factual attribution at confirmation.
    pub allocations: Vec<CollectionAllocation>,
    /// Maximum fee authorized by an account-model sweep. UTXO batches and gas
    /// funding leave this empty. Confirmation still uses the factual IX fee.
    pub fee_limit: Option<Decimal>,
    pub signed_at: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptBroadcast {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub transaction_id: TransactionRef,
    pub accepted_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachWatch {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub watch_id: WatchId,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmLeg {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub transaction_id: TransactionRef,
    /// Required for a sweep and forbidden for a gas-funding leg.
    pub allocation: Option<CollectionAllocation>,
    pub confirmed_at: u64,
}

/// Atomic lifecycle transition coupled to one mirrored IX projection for a
/// UTXO batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UtxoBatchProjectionTransition {
    /// Canonical re-inclusion of the exact retained transaction after a reorg.
    /// The durable leg returns to `Broadcast` without changing bytes,
    /// allocations, or resource ownership.
    Reincluded { included_at: u64 },
    Confirmed {
        allocations: Vec<CollectionAllocation>,
        confirmed_at: u64,
    },
    Reorged {
        error: CollectionError,
        reorged_at: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailLeg {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub transaction_id: TransactionRef,
    pub error: CollectionError,
    pub failed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReorgLeg {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub transaction_id: TransactionRef,
    pub error: CollectionError,
    pub reorged_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryLeg {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
    pub expected: TransitionGuard,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseReservation {
    pub collection_id: CollectionId,
    pub expected_collection_state: CollectionState,
    pub expected_reservation_state: CollectionReservationState,
    pub reason: ReservationReleaseReason,
    pub released_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegRef {
    pub collection_id: CollectionId,
    pub leg_id: LegId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionQuery {
    pub after: Option<CollectionId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionPage {
    pub collections: Vec<Collection>,
    pub next: Option<CollectionId>,
}

/// Durable PS collection repository. Every mutating operation is optimistic
/// and exact retries return the already-persisted result.
pub trait CollectionCreator: Send + Sync {
    fn create_or_replay_collection<'a>(
        &'a self,
        command: CollectionPlan,
    ) -> BoxFuture<'a, Result<CreateCollectionOutcome, DepositError>>;

    fn create_or_replay_utxo_batch<'a>(
        &'a self,
        command: CreateBatch,
    ) -> BoxFuture<'a, Result<CreateCollectionOutcome, DepositError>>;
}

pub trait CollectionReader: Send + Sync {
    fn collection<'a>(
        &'a self,
        id: &'a CollectionId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>>;

    /// Resolves the collection currently holding an active reservation for
    /// one deposit/asset pair. Implementations validate the ownership index
    /// and return `None` for absent or already-consumed ownership.
    fn active_collection_for<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        asset: &'a AssetId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>>;

    /// Resolves active ownership plus confirmed UTXO ownership retained for
    /// deterministic reorg handling. Released ownership is never returned.
    fn retained_collection_for<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        asset: &'a AssetId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>>;
}

pub trait CollectionHistory: Send + Sync {
    fn collections_for_deposit<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        request: CollectionQuery,
    ) -> BoxFuture<'a, Result<CollectionPage, DepositError>>;

    fn leg_for_transaction<'a>(
        &'a self,
        transaction_id: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Option<LegRef>, DepositError>>;

    fn signed_envelope<'a>(
        &'a self,
        collection_id: &'a CollectionId,
        leg_id: &'a LegId,
    ) -> BoxFuture<'a, Result<Option<SignedEnvelope>, DepositError>>;
}

pub trait SubmissionWriter: Send + Sync {
    fn record_signed<'a>(
        &'a self,
        command: RecordSignature,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn accept_broadcast<'a>(
        &'a self,
        command: AcceptBroadcast,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn attach_watch<'a>(
        &'a self,
        command: AttachWatch,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;
}

pub trait LegOutcome: Send + Sync {
    fn confirm_leg<'a>(
        &'a self,
        command: ConfirmLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn fail_leg<'a>(&'a self, command: FailLeg) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn reorg_leg<'a>(
        &'a self,
        command: ReorgLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;
}

pub trait CollectionRetry: Send + Sync {
    fn retry_leg<'a>(
        &'a self,
        command: RetryLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn release_reservation<'a>(
        &'a self,
        command: ReleaseReservation,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;
}

pub trait Collections:
    CollectionCreator
    + CollectionReader
    + CollectionHistory
    + SubmissionWriter
    + LegOutcome
    + CollectionRetry
{
}

impl<T> Collections for T where
    T: CollectionCreator
        + CollectionReader
        + CollectionHistory
        + SubmissionWriter
        + LegOutcome
        + CollectionRetry
{
}

fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: crate::DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}
