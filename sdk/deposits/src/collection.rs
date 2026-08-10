use crate::{BoxFuture, DepositError, DepositId, JobId, LedgerEntryId, PolicyIdentity, UserId};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId};
use indexing::WatchId;
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
pub struct CollectionLegId(pub String);

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
    Signed {
        transaction_id: CanonicalTransactionId,
    },
    Broadcast {
        transaction_id: CanonicalTransactionId,
    },
    Confirmed {
        transaction_id: CanonicalTransactionId,
    },
    Failed {
        transaction_id: CanonicalTransactionId,
    },
    Reorged {
        transaction_id: CanonicalTransactionId,
    },
}

impl CollectionLegState {
    #[must_use]
    pub const fn transaction_id(&self) -> Option<&CanonicalTransactionId> {
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
pub struct SafeCollectionError {
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
        transaction_id: CanonicalTransactionId,
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
    pub amount: AtomicAmount,
    pub state: CollectionReservationState,
}

/// Exact UTXO spend-resource identity. Output indexes are interpreted in the
/// transaction identified by `transaction_id`; string-only or address-level
/// reservations are deliberately insufficient for Bitcoin collection.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionSpendResourceId {
    pub transaction_id: CanonicalTransactionId,
    pub output_index: u32,
}

/// Bounded opaque evidence retained with an exact spend-resource reservation.
/// Its redacting `Debug` implementation reports only byte length so chain
/// evidence cannot enter ordinary diagnostic output.
#[derive(Clone, PartialEq, Eq)]
pub struct CollectionSpendResourceEvidence(Vec<u8>);

impl fmt::Debug for CollectionSpendResourceEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CollectionSpendResourceEvidence")
            .field("byte_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl CollectionSpendResourceEvidence {
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
pub struct CollectionSpendResource {
    pub id: CollectionSpendResourceId,
    pub amount: AtomicAmount,
    pub evidence: CollectionSpendResourceEvidence,
}

/// One deposit participating in a collection aggregate.
///
/// Account-model collections contain exactly one participant with no exact
/// spend resources. A Bitcoin UTXO batch contains one or more participants,
/// each with one or more canonically ordered outpoint reservations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionParticipant {
    pub user_id: UserId,
    pub reservation: CollectionReservation,
    pub spend_resources: Vec<CollectionSpendResource>,
}

/// Confirmed factual attribution for the sweep transaction.
///
/// `allocated_fee_asset` may differ from `asset` for token collection, where
/// the token debit and native network fee are deliberately separate values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionAllocation {
    pub deposit_id: DepositId,
    pub asset: AssetId,
    pub gross_debit: AtomicAmount,
    pub master_credit: AtomicAmount,
    pub allocated_fee_asset: AssetId,
    pub allocated_fee: AtomicAmount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionLeg {
    pub id: CollectionLegId,
    /// Stable zero-based execution order within the aggregate.
    pub position: u16,
    pub kind: CollectionLegKind,
    /// PS-owned amount fixed before execution when this leg transfers value
    /// outside the reserved collection asset. Ethereum gas funding uses this
    /// field so a restart cannot silently change the prefund amount after the
    /// durable workflow has been created. Sweep value is represented by the
    /// aggregate reservation and therefore leaves this field empty.
    pub planned_amount: Option<AtomicAmount>,
    pub state: CollectionLegState,
    /// Durable IX watch registration, retained after terminal state changes.
    pub watch_id: Option<WatchId>,
    pub attempt_count: u32,
    /// Compatibility projection for one-source account-model callers. It is
    /// `Some` exactly when `allocations` contains one item.
    pub allocation: Option<CollectionAllocation>,
    /// Factual per-deposit attribution. Bitcoin batches require one allocation
    /// for every participant in canonical participant order.
    pub allocations: Vec<CollectionAllocation>,
    pub last_error: Option<SafeCollectionError>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: CollectionId,
    /// Stable association with the durable command job that created it.
    pub job_id: JobId,
    pub user_id: UserId,
    pub deposit_id: DepositId,
    pub mode: CollectionMode,
    pub asset: AssetId,
    pub destination: CanonicalAddress,
    pub policy: PolicyIdentity,
    pub state: CollectionState,
    pub reservation: CollectionReservation,
    /// Canonically ordered participant reservations. The legacy
    /// `user_id`/`deposit_id`/`reservation` fields mirror the first participant
    /// so existing one-source Ethereum callers remain source compatible.
    pub participants: Vec<CollectionParticipant>,
    pub legs: Vec<CollectionLeg>,
    pub attempt_count: u32,
    pub last_error: Option<SafeCollectionError>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Collection {
    #[must_use]
    pub fn participant(&self, deposit_id: &DepositId) -> Option<&CollectionParticipant> {
        self.participants
            .iter()
            .find(|participant| &participant.reservation.deposit_id == deposit_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateCollectionLeg {
    pub id: CollectionLegId,
    pub kind: CollectionLegKind,
    /// Required and positive for `GasFunding`; forbidden for `Sweep`.
    pub planned_amount: Option<AtomicAmount>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateCollection {
    pub id: CollectionId,
    pub job_id: JobId,
    pub user_id: UserId,
    pub deposit_id: DepositId,
    pub mode: CollectionMode,
    pub asset: AssetId,
    pub destination: CanonicalAddress,
    pub policy: PolicyIdentity,
    pub reservation_amount: AtomicAmount,
    pub legs: Vec<CreateCollectionLeg>,
    pub created_at: u64,
}

/// One participant supplied when atomically creating a Bitcoin UTXO batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateUtxoBatchParticipant {
    pub user_id: UserId,
    pub deposit_id: DepositId,
    /// Ledger head against which the caller selected and approved these exact
    /// spend resources. Creation atomically fences every participant head so
    /// an intervening IX projection cannot leave a stale UTXO reservation.
    pub expected_ledger_head: LedgerEntryId,
    pub reservation_amount: AtomicAmount,
    pub spend_resources: Vec<CollectionSpendResource>,
}

/// Creates one Bitcoin collection aggregate over an exact, canonically
/// ordered set of participant outpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateUtxoBatchCollection {
    pub id: CollectionId,
    pub job_id: JobId,
    pub asset: AssetId,
    pub destination: CanonicalAddress,
    pub policy: PolicyIdentity,
    pub participants: Vec<CreateUtxoBatchParticipant>,
    pub leg: CreateCollectionLeg,
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
pub struct CollectionTransitionGuard {
    pub collection_state: CollectionState,
    pub leg_state: CollectionLegState,
}

/// Opaque signed chain-native bytes. This type intentionally has no `Debug`
/// implementation so envelopes cannot enter ordinary diagnostic output.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedEnvelopeBytes(Vec<u8>);

impl SignedEnvelopeBytes {
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
pub struct SignedCollectionEnvelope {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected_transaction_id: CanonicalTransactionId,
    pub bytes: SignedEnvelopeBytes,
    pub signed_at: u64,
    pub expires_at: u64,
}

/// Persists the signed envelope, transaction attribution index, and `Signed`
/// leg state in one atomic write.
/// This command intentionally has no `Debug` implementation.
pub struct RecordSignedCollectionLeg {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub expected_transaction_id: CanonicalTransactionId,
    pub envelope: SignedEnvelopeBytes,
    /// Required, canonical, and one-per-participant for UTXO batches. Other
    /// modes leave this empty and attach factual attribution at confirmation.
    pub allocations: Vec<CollectionAllocation>,
    pub signed_at: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptCollectionBroadcast {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub transaction_id: CanonicalTransactionId,
    pub accepted_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachCollectionWatch {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub watch_id: WatchId,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmCollectionLeg {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub transaction_id: CanonicalTransactionId,
    /// Required for a sweep and forbidden for a gas-funding leg.
    pub allocation: Option<CollectionAllocation>,
    pub confirmed_at: u64,
}

/// Atomic lifecycle transition coupled to one mirrored IX projection for a
/// Bitcoin UTXO batch.
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
        error: SafeCollectionError,
        reorged_at: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailCollectionLeg {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub transaction_id: CanonicalTransactionId,
    pub error: SafeCollectionError,
    pub failed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReorgCollectionLeg {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub transaction_id: CanonicalTransactionId,
    pub error: SafeCollectionError,
    pub reorged_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryCollectionLeg {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
    pub expected: CollectionTransitionGuard,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseCollectionReservation {
    pub collection_id: CollectionId,
    pub expected_collection_state: CollectionState,
    pub expected_reservation_state: CollectionReservationState,
    pub reason: ReservationReleaseReason,
    pub released_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionLegReference {
    pub collection_id: CollectionId,
    pub leg_id: CollectionLegId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionPageRequest {
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
pub trait CollectionStore: Send + Sync {
    fn create_or_replay_collection<'a>(
        &'a self,
        command: CreateCollection,
    ) -> BoxFuture<'a, Result<CreateCollectionOutcome, DepositError>>;

    fn create_or_replay_utxo_batch<'a>(
        &'a self,
        command: CreateUtxoBatchCollection,
    ) -> BoxFuture<'a, Result<CreateCollectionOutcome, DepositError>>;

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

    fn collections_for_deposit<'a>(
        &'a self,
        deposit_id: &'a DepositId,
        request: CollectionPageRequest,
    ) -> BoxFuture<'a, Result<CollectionPage, DepositError>>;

    fn leg_for_transaction<'a>(
        &'a self,
        transaction_id: &'a CanonicalTransactionId,
    ) -> BoxFuture<'a, Result<Option<CollectionLegReference>, DepositError>>;

    fn signed_envelope<'a>(
        &'a self,
        collection_id: &'a CollectionId,
        leg_id: &'a CollectionLegId,
    ) -> BoxFuture<'a, Result<Option<SignedCollectionEnvelope>, DepositError>>;

    fn record_signed<'a>(
        &'a self,
        command: RecordSignedCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn accept_broadcast<'a>(
        &'a self,
        command: AcceptCollectionBroadcast,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn attach_watch<'a>(
        &'a self,
        command: AttachCollectionWatch,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn confirm_leg<'a>(
        &'a self,
        command: ConfirmCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn fail_leg<'a>(
        &'a self,
        command: FailCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn reorg_leg<'a>(
        &'a self,
        command: ReorgCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn retry_leg<'a>(
        &'a self,
        command: RetryCollectionLeg,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;

    fn release_reservation<'a>(
        &'a self,
        command: ReleaseCollectionReservation,
    ) -> BoxFuture<'a, Result<Collection, DepositError>>;
}

fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: crate::DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}
