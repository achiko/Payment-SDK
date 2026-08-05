use crate::{BoxFuture, DepositError, DepositId, JobId, PolicyIdentity, UserId};
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId};
use indexing::WatchId;

/// Maximum opaque signed transaction size accepted by PS persistence.
pub const MAX_SIGNED_ENVELOPE_BYTES: usize = 4 * 1024 * 1024;

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
    pub allocation: Option<CollectionAllocation>,
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
    pub legs: Vec<CollectionLeg>,
    pub attempt_count: u32,
    pub last_error: Option<SafeCollectionError>,
    pub created_at: u64,
    pub updated_at: u64,
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

/// Durable recovery envelope persisted before any broadcast attempt and
/// deleted atomically when broadcast is accepted. `expires_at` is an
/// operational retention/alerting hint; it must not force PS to sign different
/// bytes while the original broadcast outcome is unknown. This type
/// intentionally has no `Debug` implementation.
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

    fn collection<'a>(
        &'a self,
        id: &'a CollectionId,
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
