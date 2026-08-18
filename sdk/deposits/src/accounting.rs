use std::{error::Error, fmt};

use crate::{BoxFuture, CaseId, CommandIdentity, DepositError, DepositId, IdempotencyKey};
use base::{Decimal, DecimalError};
use indexing::{EventId, MovementId, ObservationRevision, TransactionStatus};

use crate::amount;

/// Absolute balances after one ledger transition. Every ledger row stores the
/// complete snapshot; these are not deltas or mutable columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositBalances {
    /// Canonically included incoming value, even if not deep enough yet.
    pub received: Decimal,
    /// Subset of `received` that has satisfied IX confirmation/finality policy.
    pub confirmed: Decimal,
    /// Current canonical on-chain value at the deposit address for this asset.
    pub balance: Decimal,
    /// Confirmed gross value removed from the deposit by PS-owned collections.
    pub collected: Decimal,
    /// Value credited to the user's business account by an explicit PS decision.
    pub accounted: Decimal,
}

impl Default for DepositBalances {
    fn default() -> Self {
        Self {
            received: Decimal::zero(),
            confirmed: Decimal::zero(),
            balance: Decimal::zero(),
            collected: Decimal::zero(),
            accounted: Decimal::zero(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectionId(pub String);

impl ProjectionId {
    /// Builds the stable idempotency identity for one IX revision and deposit.
    /// Length-prefixing prevents delimiter ambiguity in opaque IDs.
    #[must_use]
    pub fn for_observation(
        event_id: &EventId,
        revision: ObservationRevision,
        deposit_id: &DepositId,
    ) -> Self {
        Self(format!(
            "ix:{}:{}:{}:{}:{}",
            event_id.0.len(),
            event_id.0,
            revision.0,
            deposit_id.0.len(),
            deposit_id.0
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerObservationKind {
    Incoming,
    Collection,
    GasFunding,
    OtherBalanceChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BalanceDirection {
    Credit,
    Debit,
}

/// A classified PS balance effect. Before persistence, `T` is a stable IX
/// [`MovementId`]; the transition engine receives amounts resolved from the
/// mirrored IX event, never amounts supplied independently by an API caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerEffect<T> {
    Incoming {
        movements: Vec<T>,
    },
    Collection {
        movements: Vec<T>,
    },
    GasFunding {
        movements: Vec<T>,
    },
    OtherBalanceChange {
        direction: BalanceDirection,
        movements: Vec<T>,
    },
    /// A factual mixed spend/return whose inputs and outputs are both visible
    /// at the same deposit. The repository resolves both sets from the mirror;
    /// the pure ledger applies their checked absolute net in either direction,
    /// or appends an unchanged snapshot when they are equal.
    NetBalanceChange {
        debit_movements: Vec<T>,
        credit_movements: Vec<T>,
    },
}

impl<T> LedgerEffect<T> {
    #[must_use]
    pub const fn kind(&self) -> LedgerObservationKind {
        match self {
            Self::Incoming { .. } => LedgerObservationKind::Incoming,
            Self::Collection { .. } => LedgerObservationKind::Collection,
            Self::GasFunding { .. } => LedgerObservationKind::GasFunding,
            Self::OtherBalanceChange { .. } | Self::NetBalanceChange { .. } => {
                LedgerObservationKind::OtherBalanceChange
            }
        }
    }

    /// Returns the primary movement set. For [`Self::NetBalanceChange`]
    /// this is the debit set; use [`Self::movement_references`] when every
    /// stable IX pointer is required.
    #[must_use]
    pub fn movements(&self) -> &[T] {
        match self {
            Self::Incoming { movements }
            | Self::Collection { movements }
            | Self::GasFunding { movements }
            | Self::OtherBalanceChange { movements, .. } => movements,
            Self::NetBalanceChange {
                debit_movements, ..
            } => debit_movements,
        }
    }

    #[must_use]
    pub fn movement_references(&self) -> Vec<&T> {
        match self {
            Self::Incoming { movements }
            | Self::Collection { movements }
            | Self::GasFunding { movements }
            | Self::OtherBalanceChange { movements, .. } => movements.iter().collect(),
            Self::NetBalanceChange {
                debit_movements,
                credit_movements,
            } => debit_movements.iter().chain(credit_movements).collect(),
        }
    }
}

pub type ObservationLedgerEffect = LedgerEffect<MovementId>;

/// Fully resolved input to the pure ledger engine. Repositories construct this
/// from a durable IX revision plus PS classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerTransition {
    pub status: TransactionStatus,
    pub previous_status: Option<TransactionStatus>,
    pub effect: LedgerEffect<Decimal>,
    /// Network fee resolved from the mirrored IX fact when the deposit address
    /// is the payer and the fee asset is the deposit asset.
    pub network_fee: Option<Decimal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerBalanceField {
    Received,
    Confirmed,
    Balance,
    Collected,
}

impl fmt::Display for LedgerBalanceField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Received => "received",
            Self::Confirmed => "confirmed",
            Self::Balance => "balance",
            Self::Collected => "collected",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerArithmeticOperation {
    Add,
    Subtract,
}

impl fmt::Display for LedgerArithmeticOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Add => "add",
            Self::Subtract => "subtract",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerTransitionError {
    EmptyEffect,
    MissingPreviousStatus,
    Arithmetic {
        field: LedgerBalanceField,
        operation: LedgerArithmeticOperation,
        source: DecimalError,
    },
}

impl fmt::Display for LedgerTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEffect => formatter.write_str(
                "an observation ledger effect must contain at least one movement amount or an eligible network fee",
            ),
            Self::MissingPreviousStatus => formatter.write_str(
                "a terminal observation status must identify the previous status to reverse",
            ),
            Self::Arithmetic {
                field,
                operation,
                source,
            } => write!(
                formatter,
                "cannot {operation} the observation amount for ledger field `{field}`: {source}"
            ),
        }
    }
}

impl Error for LedgerTransitionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Arithmetic { source, .. } => Some(source),
            Self::EmptyEffect | Self::MissingPreviousStatus => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CanonicalPhase {
    Absent,
    Included,
    Confirmed,
}

fn canonical_phase(status: &TransactionStatus) -> CanonicalPhase {
    match status {
        TransactionStatus::Included { .. } => CanonicalPhase::Included,
        TransactionStatus::Confirmed { .. } => CanonicalPhase::Confirmed,
        TransactionStatus::Pending
        | TransactionStatus::Failed { .. }
        | TransactionStatus::Replaced { .. }
        | TransactionStatus::Dropped
        | TransactionStatus::Reorged { .. } => CanonicalPhase::Absent,
    }
}

fn is_terminal(status: &TransactionStatus) -> bool {
    matches!(
        status,
        TransactionStatus::Failed { block: None, .. }
            | TransactionStatus::Replaced { .. }
            | TransactionStatus::Dropped
            | TransactionStatus::Reorged { .. }
    )
}

fn movement_total(
    effect: &LedgerEffect<Decimal>,
) -> Result<Option<Decimal>, LedgerTransitionError> {
    fn sum(amounts: &[Decimal]) -> Result<Option<Decimal>, LedgerTransitionError> {
        let Some((first, rest)) = amounts.split_first() else {
            return Ok(None);
        };
        rest.iter()
            .try_fold(first.clone(), |total, value| {
                amount::checked_add(&total, value).map_err(|source| {
                    LedgerTransitionError::Arithmetic {
                        field: LedgerBalanceField::Balance,
                        operation: LedgerArithmeticOperation::Add,
                        source,
                    }
                })
            })
            .map(Some)
    }

    let LedgerEffect::NetBalanceChange {
        debit_movements,
        credit_movements,
    } = effect
    else {
        return sum(effect.movements());
    };
    let Some(debits) = sum(debit_movements)? else {
        return Ok(None);
    };
    let credits = sum(credit_movements)?.unwrap_or_else(Decimal::zero);
    let net = amount::checked_sub(&debits, &credits)
        .or_else(|_| amount::checked_sub(&credits, &debits))
        .map_err(|source| LedgerTransitionError::Arithmetic {
            field: LedgerBalanceField::Balance,
            operation: LedgerArithmeticOperation::Subtract,
            source,
        })?;
    Ok(Some(net))
}

fn net_balance_direction(
    effect: &LedgerEffect<Decimal>,
) -> Result<Option<BalanceDirection>, LedgerTransitionError> {
    let LedgerEffect::NetBalanceChange {
        debit_movements,
        credit_movements,
    } = effect
    else {
        return Ok(None);
    };
    let sum = |amounts: &[Decimal]| {
        amounts.iter().try_fold(Decimal::zero(), |total, value| {
            amount::checked_add(&total, value).map_err(|source| LedgerTransitionError::Arithmetic {
                field: LedgerBalanceField::Balance,
                operation: LedgerArithmeticOperation::Add,
                source,
            })
        })
    };
    let debits = sum(debit_movements)?;
    let credits = sum(credit_movements)?;
    if debits == credits {
        Ok(None)
    } else if amount::checked_sub(&debits, &credits).is_ok() {
        Ok(Some(BalanceDirection::Debit))
    } else {
        Ok(Some(BalanceDirection::Credit))
    }
}

fn update_field(
    value: &mut Decimal,
    amount: &Decimal,
    field: LedgerBalanceField,
    operation: LedgerArithmeticOperation,
) -> Result<(), LedgerTransitionError> {
    *value = match operation {
        LedgerArithmeticOperation::Add => amount::checked_add(value, amount),
        LedgerArithmeticOperation::Subtract => amount::checked_sub(value, amount),
    }
    .map_err(|source| LedgerTransitionError::Arithmetic {
        field,
        operation,
        source,
    })?;
    Ok(())
}

impl LedgerArithmeticOperation {
    fn inverse(self) -> Self {
        match self {
            Self::Add => Self::Subtract,
            Self::Subtract => Self::Add,
        }
    }
}

fn contribution_operations(
    effect: &LedgerEffect<Decimal>,
    phase: CanonicalPhase,
) -> Result<Vec<(LedgerBalanceField, LedgerArithmeticOperation)>, LedgerTransitionError> {
    use LedgerArithmeticOperation::{Add, Subtract};
    use LedgerBalanceField::{Balance, Collected, Confirmed, Received};

    let operations = match (effect, phase) {
        (_, CanonicalPhase::Absent) => Vec::new(),
        (LedgerEffect::Incoming { .. }, CanonicalPhase::Included) => {
            vec![(Received, Add), (Balance, Add)]
        }
        (LedgerEffect::Incoming { .. }, CanonicalPhase::Confirmed) => {
            vec![(Received, Add), (Confirmed, Add), (Balance, Add)]
        }
        (LedgerEffect::Collection { .. }, CanonicalPhase::Included) => {
            vec![(Balance, Subtract)]
        }
        (LedgerEffect::Collection { .. }, CanonicalPhase::Confirmed) => {
            vec![(Balance, Subtract), (Collected, Add)]
        }
        (LedgerEffect::GasFunding { .. }, CanonicalPhase::Included | CanonicalPhase::Confirmed) => {
            vec![(Balance, Add)]
        }
        (
            LedgerEffect::OtherBalanceChange {
                direction: BalanceDirection::Credit,
                ..
            },
            CanonicalPhase::Included | CanonicalPhase::Confirmed,
        ) => vec![(Balance, Add)],
        (
            LedgerEffect::OtherBalanceChange {
                direction: BalanceDirection::Debit,
                ..
            },
            CanonicalPhase::Included | CanonicalPhase::Confirmed,
        ) => vec![(Balance, Subtract)],
        (
            LedgerEffect::NetBalanceChange { .. },
            CanonicalPhase::Included | CanonicalPhase::Confirmed,
        ) => match net_balance_direction(effect)? {
            Some(BalanceDirection::Credit) => vec![(Balance, Add)],
            Some(BalanceDirection::Debit) => vec![(Balance, Subtract)],
            None => Vec::new(),
        },
    };
    Ok(operations)
}

fn network_fee_operations(
    kind: LedgerObservationKind,
    status: &TransactionStatus,
) -> Vec<(LedgerBalanceField, LedgerArithmeticOperation)> {
    use LedgerArithmeticOperation::{Add, Subtract};
    use LedgerBalanceField::{Balance, Collected};

    let canonical = matches!(
        status,
        TransactionStatus::Included { .. }
            | TransactionStatus::Confirmed { .. }
            | TransactionStatus::Failed { block: Some(_), .. }
    );
    if !canonical {
        return Vec::new();
    }

    let mut operations = vec![(Balance, Subtract)];
    if kind == LedgerObservationKind::Collection
        && matches!(status, TransactionStatus::Confirmed { .. })
    {
        operations.push((Collected, Add));
    }
    operations
}

fn apply_operations(
    balances: &mut DepositBalances,
    amount: &Decimal,
    operations: impl IntoIterator<Item = (LedgerBalanceField, LedgerArithmeticOperation)>,
) -> Result<(), LedgerTransitionError> {
    for (field, operation) in operations {
        let value = match field {
            LedgerBalanceField::Received => &mut balances.received,
            LedgerBalanceField::Confirmed => &mut balances.confirmed,
            LedgerBalanceField::Balance => &mut balances.balance,
            LedgerBalanceField::Collected => &mut balances.collected,
        };
        update_field(value, amount, field, operation)?;
    }
    Ok(())
}

/// Applies one IX revision to a complete absolute ledger snapshot.
///
/// The prior status contribution is removed before the new status contribution
/// is applied. Movements and an eligible network fee are tracked separately:
/// the fee debits canonical balance for included, confirmed, and block-backed
/// failed observations, while only a confirmed collection adds it to
/// `collected`. This makes confirmation, failure, reorg, and re-inclusion
/// deterministic without trusting a caller-supplied absolute balance.
/// `accounted` is copied unchanged.
///
/// # Errors
///
/// Returns a structured error when neither movements nor a fee are present,
/// when a non-canonical terminal revision lacks a prior status, or on any
/// unsigned 256-bit overflow/underflow.
pub fn apply_observation_transition(
    current: DepositBalances,
    transition: &LedgerTransition,
) -> Result<DepositBalances, LedgerTransitionError> {
    if is_terminal(&transition.status) && transition.previous_status.is_none() {
        return Err(LedgerTransitionError::MissingPreviousStatus);
    }

    let movement_total = movement_total(&transition.effect)?;
    if movement_total.is_none() && transition.network_fee.is_none() {
        return Err(LedgerTransitionError::EmptyEffect);
    }

    let mut next = current;
    if let Some(previous_status) = transition.previous_status.as_ref() {
        if let Some(amount) = movement_total.as_ref() {
            let removal =
                contribution_operations(&transition.effect, canonical_phase(previous_status))?
                    .into_iter()
                    .map(|(field, operation)| (field, operation.inverse()));
            apply_operations(&mut next, amount, removal)?;
        }
        if let Some(network_fee) = transition.network_fee.as_ref() {
            let removal = network_fee_operations(transition.effect.kind(), previous_status)
                .into_iter()
                .map(|(field, operation)| (field, operation.inverse()));
            apply_operations(&mut next, network_fee, removal)?;
        }
    }
    if let Some(amount) = movement_total.as_ref() {
        apply_operations(
            &mut next,
            amount,
            contribution_operations(&transition.effect, canonical_phase(&transition.status))?,
        )?;
    }
    if let Some(network_fee) = transition.network_fee.as_ref() {
        apply_operations(
            &mut next,
            network_fee,
            network_fee_operations(transition.effect.kind(), &transition.status),
        )?;
    }
    Ok(next)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerEntryCause {
    Opened {
        idempotency_key: IdempotencyKey,
    },
    Observation {
        projection_id: ProjectionId,
        event_id: EventId,
        observation_revision: ObservationRevision,
        status: TransactionStatus,
        kind: LedgerObservationKind,
        /// Stable pointers into the mirrored IX fact that changed this deposit.
        movement_ids: Vec<MovementId>,
        /// Eligible fee amount after repository validation of the mirrored
        /// fee asset and payer. The status determines whether it contributes.
        network_fee: Option<Decimal>,
    },
    Accounting {
        idempotency_key: IdempotencyKey,
        reason: String,
    },
    /// Explicit business correction made while resolving a durable
    /// reconciliation case. Only `accounted` may differ from the previous
    /// absolute snapshot.
    ReconciliationResolution {
        case_id: CaseId,
        idempotency_key: IdempotencyKey,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenLedger {
    pub idempotency_key: IdempotencyKey,
    pub deposit_id: DepositId,
    pub recorded_at: u64,
}

/// Immutable PS ledger row. `previous` makes the per-deposit journal a
/// verifiable sequence and supplies an optimistic-concurrency boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    pub id: EntryId,
    pub deposit_id: DepositId,
    pub previous: Option<EntryId>,
    pub cause: LedgerEntryCause,
    pub balances: DepositBalances,
    pub recorded_at: u64,
}

/// PS classification for one deposit and mirrored IX event. Callers identify
/// relevant movement IDs; persistence resolves all amounts and status history
/// from the immutable mirror and computes the absolute snapshot itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordObservation {
    pub event_id: EventId,
    pub effect: ObservationLedgerEffect,
    pub deposit_id: DepositId,
    pub expected_head: Option<EntryId>,
    pub recorded_at: u64,
}

/// The ledger copies all on-chain balances from the current head and changes
/// only `accounted`, then appends a new absolute snapshot row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountingCommand {
    /// Scoped caller idempotency. `operation` must be
    /// [`crate::CommandOperation::Accounting`].
    pub command: CommandIdentity,
    pub deposit_id: DepositId,
    pub expected_head: Option<EntryId>,
    pub next_accounted: Decimal,
    /// Human-readable administrator/business justification retained in the
    /// immutable audit row. It must be non-blank and at most 1,024 bytes.
    pub reason: String,
    pub recorded_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    Appended { entry: LedgerEntry },
    AlreadyPresent { entry: LedgerEntry },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerQuery {
    pub deposit_id: DepositId,
    pub after: Option<EntryId>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerPage {
    pub entries: Vec<LedgerEntry>,
    pub next: Option<EntryId>,
}

pub trait LedgerWriter: Send + Sync {
    /// Idempotently creates the zero-balance first row for a persisted deposit.
    fn open<'a>(&'a self, command: OpenLedger) -> BoxFuture<'a, Result<ApplyResult, DepositError>>;

    /// Resolves and appends the absolute snapshot. Implementations must
    /// preserve `accounted`, apply optimistic head matching, and derive IX
    /// status, revision, movement amounts, and eligible network fee from the
    /// durable event mirror.
    fn record_observation<'a>(
        &'a self,
        command: RecordObservation,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>>;

    /// Appends a new absolute row by copying on-chain fields from the current
    /// head and changing only `accounted`. A positive value must not exceed the
    /// confirmation-qualified amount at authorization time.
    fn record_accounting<'a>(
        &'a self,
        command: AccountingCommand,
    ) -> BoxFuture<'a, Result<ApplyResult, DepositError>>;
}

pub trait LedgerReader: Send + Sync {
    fn current<'a>(
        &'a self,
        deposit_id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<LedgerEntry>, DepositError>>;

    fn entries<'a>(
        &'a self,
        request: LedgerQuery,
    ) -> BoxFuture<'a, Result<LedgerPage, DepositError>>;
}

pub trait DepositLedger: LedgerWriter + LedgerReader {}

impl<T> DepositLedger for T where T: LedgerWriter + LedgerReader {}

#[cfg(test)]
mod tests {
    use super::*;
    use indexing::{BlockHash, BlockHeight, BlockRef, ConfirmationProof};
    use indexing::{ChainId, TransactionRef};

    fn amount(value: u64) -> Decimal {
        Decimal::from(value)
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8; 32]),
            parent_hash: None,
            timestamp: None,
        }
    }

    fn included() -> TransactionStatus {
        TransactionStatus::Included {
            block: block(1),
            confirmations: 1,
        }
    }

    fn confirmed() -> TransactionStatus {
        TransactionStatus::Confirmed {
            block: block(1),
            proof: ConfirmationProof::Depth {
                required: 12,
                observed: 12,
            },
        }
    }

    fn incoming(
        status: TransactionStatus,
        previous_status: Option<TransactionStatus>,
        amounts: Vec<Decimal>,
    ) -> LedgerTransition {
        LedgerTransition {
            status,
            previous_status,
            effect: LedgerEffect::Incoming { movements: amounts },
            network_fee: None,
        }
    }

    #[test]
    fn incoming_confirmation_reorg_and_reinclusion_are_absolute_transitions() {
        let opened = DepositBalances::default();
        let included_balances = apply_observation_transition(
            opened,
            &incoming(included(), None, vec![amount(40), amount(60)]),
        )
        .expect("two included movements fit in the ledger");
        assert_eq!(
            included_balances.clone(),
            DepositBalances {
                received: amount(100),
                confirmed: amount(0),
                balance: amount(100),
                collected: amount(0),
                accounted: amount(0),
            }
        );

        let confirmed_balances = apply_observation_transition(
            included_balances.clone(),
            &incoming(confirmed(), Some(included()), vec![amount(40), amount(60)]),
        )
        .expect("confirmation replaces rather than duplicates inclusion");
        assert_eq!(confirmed_balances.received, amount(100));
        assert_eq!(confirmed_balances.confirmed, amount(100));
        assert_eq!(confirmed_balances.balance, amount(100));

        let reorged = apply_observation_transition(
            confirmed_balances,
            &incoming(
                TransactionStatus::Reorged {
                    previous_block: block(1),
                },
                Some(confirmed()),
                vec![amount(40), amount(60)],
            ),
        )
        .expect("reorg removes the confirmed canonical contribution");
        assert_eq!(reorged, DepositBalances::default());

        let reincluded = apply_observation_transition(
            reorged,
            &incoming(
                included(),
                Some(TransactionStatus::Reorged {
                    previous_block: block(1),
                }),
                vec![amount(40), amount(60)],
            ),
        )
        .expect("re-inclusion restores the canonical contribution once");
        assert_eq!(reincluded, included_balances);
    }

    #[test]
    fn observations_preserve_accounted_and_do_not_cap_partial_or_overpayments() {
        for paid in [40, 150] {
            let current = DepositBalances {
                accounted: amount(25),
                ..DepositBalances::default()
            };
            let next = apply_observation_transition(
                current,
                &incoming(included(), None, vec![amount(paid)]),
            )
            .expect("valid payment amount fits");
            assert_eq!(next.received, amount(paid));
            assert_eq!(next.balance, amount(paid));
            assert_eq!(next.accounted, amount(25));
        }
    }

    #[test]
    fn collected_changes_only_on_confirmation_and_reverses_on_reorg() {
        let current = DepositBalances {
            received: amount(100),
            confirmed: amount(100),
            balance: amount(100),
            collected: amount(0),
            accounted: amount(100),
        };
        let included_transition = LedgerTransition {
            status: included(),
            previous_status: None,
            effect: LedgerEffect::Collection {
                movements: vec![amount(100)],
            },
            network_fee: None,
        };
        let swept = apply_observation_transition(current.clone(), &included_transition)
            .expect("included collection can debit the current balance");
        assert_eq!(swept.balance, amount(0));
        assert_eq!(swept.collected, amount(0));
        assert_eq!(swept.accounted, amount(100));

        let confirmed_transition = LedgerTransition {
            status: confirmed(),
            previous_status: Some(included()),
            effect: LedgerEffect::Collection {
                movements: vec![amount(100)],
            },
            network_fee: None,
        };
        let confirmed_sweep = apply_observation_transition(swept, &confirmed_transition)
            .expect("confirmed collection increments gross collected once");
        assert_eq!(confirmed_sweep.balance, amount(0));
        assert_eq!(confirmed_sweep.collected, amount(100));

        let reorg_transition = LedgerTransition {
            status: TransactionStatus::Reorged {
                previous_block: block(1),
            },
            previous_status: Some(confirmed()),
            effect: LedgerEffect::Collection {
                movements: vec![amount(100)],
            },
            network_fee: None,
        };
        let restored = apply_observation_transition(confirmed_sweep, &reorg_transition)
            .expect("reorg restores balance and reverses collected");
        assert_eq!(restored, current);
    }

    #[test]
    fn overflow_and_underflow_report_the_exact_field_and_operation() {
        let maximum = amount::from_bytes([u8::MAX; 32]);
        let overflow = apply_observation_transition(
            DepositBalances {
                received: maximum,
                ..DepositBalances::default()
            },
            &incoming(included(), None, vec![amount(1)]),
        )
        .expect_err("received cannot exceed u256");
        assert_eq!(
            overflow,
            LedgerTransitionError::Arithmetic {
                field: LedgerBalanceField::Received,
                operation: LedgerArithmeticOperation::Add,
                source: DecimalError::new(
                    base::DecimalErrorKind::Overflow,
                    "atomic amount exceeds 32 bytes"
                ),
            }
        );

        let underflow = apply_observation_transition(
            DepositBalances::default(),
            &LedgerTransition {
                status: included(),
                previous_status: None,
                effect: LedgerEffect::Collection {
                    movements: vec![amount(1)],
                },
                network_fee: None,
            },
        )
        .expect_err("collection cannot debit more than the current balance");
        assert_eq!(
            underflow,
            LedgerTransitionError::Arithmetic {
                field: LedgerBalanceField::Balance,
                operation: LedgerArithmeticOperation::Subtract,
                source: DecimalError::new(
                    base::DecimalErrorKind::NegativeAmount,
                    "currency amount must not be negative"
                ),
            }
        );
    }

    #[test]
    fn terminal_transition_requires_previous_status() {
        let error = apply_observation_transition(
            DepositBalances::default(),
            &incoming(TransactionStatus::Dropped, None, vec![amount(1)]),
        )
        .expect_err("a drop without prior state cannot reverse a contribution");
        assert_eq!(error, LedgerTransitionError::MissingPreviousStatus);
    }

    #[test]
    fn failed_dropped_and_replaced_remove_the_previous_canonical_contribution() {
        let included_balances = apply_observation_transition(
            DepositBalances::default(),
            &incoming(included(), None, vec![amount(10)]),
        )
        .expect("inclusion contributes the incoming amount");
        let terminal_statuses = [
            TransactionStatus::Failed {
                block: None,
                reason: Some("execution failed".to_owned()),
            },
            TransactionStatus::Dropped,
            TransactionStatus::Replaced {
                by: TransactionRef {
                    scope: indexing::IndexScope {
                        chain: ChainId("chain-a".to_owned()),
                        network: "test".to_owned(),
                    },
                    value: "0xreplacement".to_owned(),
                },
            },
        ];

        for status in terminal_statuses {
            let corrected = apply_observation_transition(
                included_balances.clone(),
                &incoming(status, Some(included()), vec![amount(10)]),
            )
            .expect("terminal revision reverses the prior inclusion");
            assert_eq!(corrected, DepositBalances::default());
        }
    }

    #[test]
    fn collection_fee_changes_balance_immediately_and_collected_on_confirmation() {
        let current = DepositBalances {
            received: amount(110),
            confirmed: amount(110),
            balance: amount(110),
            collected: amount(0),
            accounted: amount(110),
        };
        let included_transition = LedgerTransition {
            status: included(),
            previous_status: None,
            effect: LedgerEffect::Collection {
                movements: vec![amount(100)],
            },
            network_fee: Some(amount(10)),
        };
        let included_balances = apply_observation_transition(current.clone(), &included_transition)
            .expect("included native collection debits its transfer and network fee");
        assert_eq!(included_balances.balance, amount(0));
        assert_eq!(included_balances.collected, amount(0));

        let confirmed_balances = apply_observation_transition(
            included_balances,
            &LedgerTransition {
                status: confirmed(),
                previous_status: Some(included()),
                effect: LedgerEffect::Collection {
                    movements: vec![amount(100)],
                },
                network_fee: Some(amount(10)),
            },
        )
        .expect("confirmation retains the debit and records the gross collection");
        assert_eq!(confirmed_balances.balance, amount(0));
        assert_eq!(confirmed_balances.collected, amount(110));

        let restored = apply_observation_transition(
            confirmed_balances,
            &LedgerTransition {
                status: TransactionStatus::Reorged {
                    previous_block: block(1),
                },
                previous_status: Some(confirmed()),
                effect: LedgerEffect::Collection {
                    movements: vec![amount(100)],
                },
                network_fee: Some(amount(10)),
            },
        )
        .expect("reorg reverses both the movement and fee contributions");
        assert_eq!(restored, current);
    }

    #[test]
    fn fee_only_block_failure_debits_balance_and_reorg_restores_it() {
        let current = DepositBalances {
            balance: amount(10),
            ..DepositBalances::default()
        };
        let failed = TransactionStatus::Failed {
            block: Some(block(1)),
            reason: Some("execution reverted".to_owned()),
        };
        let failed_balances = apply_observation_transition(
            current.clone(),
            &LedgerTransition {
                status: failed.clone(),
                previous_status: None,
                effect: LedgerEffect::Collection {
                    movements: Vec::new(),
                },
                network_fee: Some(amount(10)),
            },
        )
        .expect("a canonical failure may contain only a paid network fee");
        assert_eq!(failed_balances.balance, amount(0));
        assert_eq!(failed_balances.collected, amount(0));

        let restored = apply_observation_transition(
            failed_balances,
            &LedgerTransition {
                status: TransactionStatus::Reorged {
                    previous_block: block(1),
                },
                previous_status: Some(failed),
                effect: LedgerEffect::Collection {
                    movements: Vec::new(),
                },
                network_fee: Some(amount(10)),
            },
        )
        .expect("reorg reverses the canonical failure fee");
        assert_eq!(restored, current);
    }

    #[test]
    fn confirmed_non_collection_fee_never_changes_collected() {
        let next = apply_observation_transition(
            DepositBalances {
                balance: amount(10),
                ..DepositBalances::default()
            },
            &LedgerTransition {
                status: confirmed(),
                previous_status: None,
                effect: LedgerEffect::OtherBalanceChange {
                    direction: BalanceDirection::Debit,
                    movements: Vec::new(),
                },
                network_fee: Some(amount(10)),
            },
        )
        .expect("a fee-only non-collection effect can be projected");
        assert_eq!(next.balance, amount(0));
        assert_eq!(next.collected, amount(0));
    }
}
