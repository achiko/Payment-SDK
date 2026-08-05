use std::collections::{BTreeMap, BTreeSet};

use chain_identity::{AssetId, CanonicalAddress, CanonicalTransactionId, ChainId};
use indexing::{EventCursor, IndexScope, ObservationEventId};
use storage::{Namespace, ScanRequest, Storage, StorageErrorKind};

use crate::{
    AwaitingWatchPageRequest, Collection, CollectionId, CollectionLegId, CollectionLegReference,
    CollectionPageRequest, CollectionReservationState, CollectionStore, CommandOperation,
    ConsumerCheckpointName, Deposit, DepositError, DepositErrorKind, DepositId,
    DepositIndexRebuildRequest, DepositLedger, DepositObservationLogRequest, DepositPageRequest,
    DepositState, DepositStateKind, DepositStore, Job, JobId, JobPageRequest, JobPayload,
    JobResource, JobState, JobStore, LedgerEntry, LedgerEntryCause, LedgerEntryId,
    LedgerPageRequest, MirroredObservation, ObservationConsumerCheckpoints, ObservationEventLog,
    ObservationLogRequest, PersistentPaymentRepository, ProjectionId, ReconciliationCase,
    ReconciliationCaseId, ReconciliationDecision, ReconciliationPageRequest, ReconciliationState,
    ReconciliationStore, User, UserId,
};

const DEPOSIT_NS: &str = "ps.v1.deposit";
const DEPOSIT_ADDRESS_NS: &str = "ps.v1.deposit_address";
const DEPOSIT_IDEMPOTENCY_NS: &str = "ps.v1.deposit_idem";
const AWAITING_WATCH_NS: &str = "ps.v1.awaiting_watch";
const USER_DEPOSIT_NS: &str = "ps.v1.user_deposit";
const DEPOSIT_STATE_NS: &str = "ps.v1.deposit_state";
const USER_DEPOSIT_STATE_NS: &str = "ps.v1.user_deposit_state";
const DEPOSIT_INDEX_METADATA_NS: &str = "ps.v1.deposit_index_metadata";
const LEDGER_HEAD_NS: &str = "ps.v1.ledger_head";
const LEDGER_ENTRY_NS: &str = "ps.v1.ledger_entry";
const PROJECTION_NS: &str = "ps.v1.projection";
const ACCOUNTING_IDEMPOTENCY_NS: &str = "ps.v1.accounting_idem";
const OBSERVATION_NS: &str = "ps.v1.observation";
const OBSERVATION_CURSOR_NS: &str = "ps.v1.observation_cursor";
const DEPOSIT_OBSERVATION_NS: &str = "ps.v1.deposit_observation";
const CONSUMER_CHECKPOINT_NS: &str = "ps.v1.consumer_checkpoint";
const RECONCILIATION_NS: &str = "ps.v1.reconciliation";
const RECONCILIATION_DEPOSIT_NS: &str = "ps.v1.reconciliation_deposit";
const RECONCILIATION_RESOLUTION_IDEMPOTENCY_NS: &str = "ps.v1.reconciliation_resolution_idem";
const USER_NS: &str = "ps.v1.user";
const JOB_NS: &str = "ps.v1.job";
const COMMAND_JOB_NS: &str = "ps.v1.command_job";
const USER_JOB_NS: &str = "ps.v1.user_job";
const RESOURCE_JOB_NS: &str = "ps.v1.resource_job";
const READY_JOB_NS: &str = "ps.v1.ready_job";
const COLLECTION_NS: &str = "ps.v1.collection";
const COLLECTION_JOB_NS: &str = "ps.v1.collection_job";
const DEPOSIT_COLLECTION_NS: &str = "ps.v1.deposit_collection";
const ACTIVE_RESERVATION_NS: &str = "ps.v1.active_collection_reservation";
const COLLECTION_TRANSACTION_NS: &str = "ps.v1.collection_transaction";
const SIGNED_ENVELOPE_NS: &str = "ps.v1.signed_collection_envelope";

pub(crate) struct MigrationValidationReport {
    pub(crate) deposits: usize,
    pub(crate) ledger_entries: usize,
    pub(crate) mirrored_observations: usize,
    pub(crate) deposit_observations: usize,
    pub(crate) reconciliation_cases: usize,
    pub(crate) users: usize,
    pub(crate) jobs: usize,
    pub(crate) collections: usize,
    pub(crate) deposit_indexes_rebuilt: usize,
}

struct LedgerAudit {
    entries: BTreeMap<(DepositId, LedgerEntryId), LedgerEntry>,
    reconciliation_case_ids: BTreeSet<ReconciliationCaseId>,
    deposit_observations: Vec<(DepositId, EventCursor, ObservationEventId)>,
}

struct ReconciliationAudit {
    cases: BTreeMap<ReconciliationCaseId, ReconciliationCase>,
}

pub(crate) async fn validate_and_rebuild<S>(
    repository: &PersistentPaymentRepository<S>,
    scope: &IndexScope,
    page_size: usize,
) -> Result<MigrationValidationReport, DepositError>
where
    S: Storage,
{
    let users = load_users(repository, page_size).await?;
    let deposits = load_deposits(repository, scope, page_size).await?;
    if deposits
        .values()
        .any(|deposit| !users.contains_key(&deposit.user_id))
    {
        return Err(invariant("deposit references a missing user record"));
    }
    validate_deposit_core_indexes(repository, &deposits, page_size).await?;

    let observations = load_observations(repository, scope, page_size).await?;
    validate_consumer_checkpoints(repository, &observations, page_size).await?;
    let ledger = load_ledgers(repository, &deposits, &observations, page_size).await?;
    let reconciliation =
        load_reconciliation(repository, &deposits, &observations, &ledger, page_size).await?;
    let expected_reconciliation_ledger_rows = reconciliation
        .cases
        .values()
        .filter_map(|case| match &case.state {
            ReconciliationState::Resolved { resolution, .. }
                if matches!(
                    &resolution.decision,
                    ReconciliationDecision::ReverseCredit { .. }
                ) =>
            {
                Some(case.id.clone())
            }
            ReconciliationState::Open
            | ReconciliationState::LegacyResolved { .. }
            | ReconciliationState::Resolved { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    if ledger.reconciliation_case_ids != expected_reconciliation_ledger_rows {
        return Err(invariant(
            "reconciliation ledger rows and durable reconciliation cases do not match",
        ));
    }

    let jobs = load_jobs(repository, scope, &users, &deposits, page_size).await?;
    let collections =
        load_collections(repository, scope, &users, &deposits, &jobs, page_size).await?;

    validate_raw_count(repository, DEPOSIT_NS, deposits.len(), page_size).await?;
    validate_raw_count(repository, LEDGER_ENTRY_NS, ledger.entries.len(), page_size).await?;
    validate_raw_count(repository, OBSERVATION_NS, observations.len(), page_size).await?;
    validate_raw_count(
        repository,
        RECONCILIATION_NS,
        reconciliation.cases.len(),
        page_size,
    )
    .await?;
    validate_raw_count(repository, USER_NS, users.len(), page_size).await?;
    validate_raw_count(repository, JOB_NS, jobs.len(), page_size).await?;
    validate_raw_count(repository, COLLECTION_NS, collections.len(), page_size).await?;

    let deposit_observations = complete_deposit_observation_attributions(
        repository,
        &deposits,
        &observations,
        &collections,
        &ledger.deposit_observations,
        page_size,
    )
    .await?;
    let deposit_observation_count = repository
        .migration_rebuild_deposit_observation_index(&deposit_observations, page_size)
        .await?;
    validate_raw_count(
        repository,
        DEPOSIT_OBSERVATION_NS,
        deposit_observation_count,
        page_size,
    )
    .await?;

    let deposit_indexes_rebuilt = rebuild_deposit_indexes(repository, page_size).await?;
    validate_rebuilt_deposit_indexes(repository, &deposits, page_size).await?;

    Ok(MigrationValidationReport {
        deposits: deposits.len(),
        ledger_entries: ledger.entries.len(),
        mirrored_observations: observations.len(),
        deposit_observations: deposit_observation_count,
        reconciliation_cases: reconciliation.cases.len(),
        users: users.len(),
        jobs: jobs.len(),
        collections: collections.len(),
        deposit_indexes_rebuilt,
    })
}

async fn complete_deposit_observation_attributions<S>(
    repository: &PersistentPaymentRepository<S>,
    deposits: &BTreeMap<DepositId, Deposit>,
    observations: &BTreeMap<ObservationEventId, MirroredObservation>,
    collections: &BTreeMap<CollectionId, Collection>,
    ledger_attributions: &[(DepositId, EventCursor, ObservationEventId)],
    page_size: usize,
) -> Result<Vec<(DepositId, EventCursor, ObservationEventId)>, DepositError>
where
    S: Storage,
{
    let mut attributions = ledger_attributions.iter().cloned().collect::<BTreeSet<_>>();
    let mut collection_transactions =
        BTreeMap::<DepositId, BTreeSet<CanonicalTransactionId>>::new();
    for collection in collections.values() {
        for transaction_id in collection
            .legs
            .iter()
            .filter_map(|leg| leg.state.transaction_id())
        {
            collection_transactions
                .entry(collection.deposit_id.clone())
                .or_default()
                .insert(transaction_id.clone());
        }
    }

    for (deposit_id, deposit) in deposits {
        let mut after = None;
        loop {
            let page = repository
                .observations_for_deposit(DepositObservationLogRequest {
                    deposit_id: deposit_id.clone(),
                    after,
                    limit: page_size,
                })
                .await?;
            for observation in page.observations {
                let event = &observation.event;
                if observations.get(&event.id) != Some(&observation) {
                    return Err(invariant(
                        "deposit observation index references a different mirror payload",
                    ));
                }
                let address_relevant = event.transaction.movements.iter().any(|movement| {
                    movement.from.as_ref() == Some(&deposit.address)
                        || movement.to.as_ref() == Some(&deposit.address)
                }) || event
                    .transaction
                    .fee
                    .as_ref()
                    .and_then(|fee| fee.payer.as_ref())
                    == Some(&deposit.address);
                let collection_relevant =
                    collection_transactions
                        .get(deposit_id)
                        .is_some_and(|transactions| {
                            transactions.contains(&event.transaction.transaction_id)
                        });
                if !address_relevant && !collection_relevant {
                    return Err(invariant(
                        "deposit observation index retains an event unrelated to the deposit",
                    ));
                }
                attributions.insert((deposit_id.clone(), event.cursor, event.id.clone()));
            }
            let Some(next) = page.next else {
                break;
            };
            if after.is_some_and(|current| next <= current) {
                return Err(invariant("deposit observation scan cursor did not advance"));
            }
            after = Some(next);
        }
    }
    Ok(attributions.into_iter().collect())
}

async fn load_users<S>(
    repository: &PersistentPaymentRepository<S>,
    page_size: usize,
) -> Result<BTreeMap<UserId, User>, DepositError>
where
    S: Storage,
{
    let mut users = BTreeMap::new();
    for user in repository.migration_users(page_size).await? {
        if users.insert(user.id.clone(), user).is_some() {
            return Err(invariant("duplicate user ID found during migration"));
        }
    }
    Ok(users)
}

async fn load_deposits<S>(
    repository: &PersistentPaymentRepository<S>,
    scope: &IndexScope,
    page_size: usize,
) -> Result<BTreeMap<DepositId, Deposit>, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut deposits = BTreeMap::new();
    loop {
        let page = repository
            .deposits(DepositPageRequest {
                after: after.clone(),
                limit: page_size,
                user_id: None,
                state: None,
            })
            .await?;
        for deposit in page.deposits {
            ensure_asset_scope(&deposit.asset, scope, "deposit asset")?;
            ensure_address_scope(&deposit.address, scope, "deposit address")?;
            if deposit.key_purpose.trim().is_empty() {
                return Err(invariant("deposit key purpose is empty"));
            }
            let by_address = repository
                .by_address(&deposit.address)
                .await?
                .ok_or_else(|| invariant("deposit address index is missing"))?;
            if by_address != deposit {
                return Err(invariant(
                    "deposit address index points to a different deposit",
                ));
            }
            if deposits.insert(deposit.id.clone(), deposit).is_some() {
                return Err(invariant("duplicate deposit ID found during migration"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "deposit")?;
        after = Some(next);
    }
    Ok(deposits)
}

async fn validate_deposit_core_indexes<S>(
    repository: &PersistentPaymentRepository<S>,
    deposits: &BTreeMap<DepositId, Deposit>,
    page_size: usize,
) -> Result<(), DepositError>
where
    S: Storage,
{
    validate_raw_count(repository, DEPOSIT_ADDRESS_NS, deposits.len(), page_size).await?;
    validate_raw_count(
        repository,
        DEPOSIT_IDEMPOTENCY_NS,
        deposits.len(),
        page_size,
    )
    .await?;
    let deposits_to_validate = deposits.values().cloned().collect::<Vec<_>>();
    repository
        .validate_migration_deposit_idempotency_indexes(&deposits_to_validate)
        .await?;

    let expected_awaiting = deposits
        .values()
        .filter(|deposit| deposit.state == DepositState::AwaitingWatch)
        .map(|deposit| deposit.id.clone())
        .collect::<BTreeSet<_>>();
    let mut actual_awaiting = BTreeSet::new();
    let mut after = None;
    loop {
        let page = repository
            .awaiting_watch(AwaitingWatchPageRequest {
                after: after.clone(),
                limit: page_size,
            })
            .await?;
        for deposit in page.deposits {
            if !actual_awaiting.insert(deposit.id) {
                return Err(invariant("duplicate AwaitingWatch deposit index"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "AwaitingWatch deposit")?;
        after = Some(next);
    }
    if actual_awaiting != expected_awaiting {
        return Err(invariant(
            "AwaitingWatch index does not match authoritative deposit states",
        ));
    }
    validate_raw_count(
        repository,
        AWAITING_WATCH_NS,
        expected_awaiting.len(),
        page_size,
    )
    .await
}

async fn load_observations<S>(
    repository: &PersistentPaymentRepository<S>,
    scope: &IndexScope,
    page_size: usize,
) -> Result<BTreeMap<ObservationEventId, MirroredObservation>, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut observations = BTreeMap::new();
    let mut cursors = BTreeSet::new();
    loop {
        let page = repository
            .observations(ObservationLogRequest {
                after,
                limit: page_size,
            })
            .await?;
        for observation in page.observations {
            let event = &observation.event;
            if &event.transaction.scope != scope {
                return Err(conflict(
                    "mirrored IX observation belongs to a different chain/network scope",
                ));
            }
            ensure_transaction_scope(
                &event.transaction.transaction_id,
                scope,
                "mirrored transaction",
            )?;
            let mut movement_ids = BTreeSet::new();
            for movement in &event.transaction.movements {
                ensure_asset_scope(&movement.asset, scope, "mirrored movement asset")?;
                if let Some(from) = &movement.from {
                    ensure_address_scope(from, scope, "mirrored movement sender")?;
                }
                if let Some(to) = &movement.to {
                    ensure_address_scope(to, scope, "mirrored movement recipient")?;
                }
                if !movement_ids.insert(movement.id.clone()) {
                    return Err(invariant(
                        "mirrored observation contains duplicate movement IDs",
                    ));
                }
            }
            if let Some(fee) = &event.transaction.fee {
                ensure_asset_scope(&fee.asset, scope, "mirrored network-fee asset")?;
                if let Some(payer) = &fee.payer {
                    ensure_address_scope(payer, scope, "mirrored network-fee payer")?;
                }
            }
            let direct = repository
                .observation(&event.id)
                .await?
                .ok_or_else(|| invariant("mirrored observation row is not addressable by ID"))?;
            if direct != observation {
                return Err(invariant(
                    "mirrored observation ID points to a different event",
                ));
            }
            if !cursors.insert(event.cursor) {
                return Err(invariant("duplicate mirrored observation cursor"));
            }
            if observations.insert(event.id.clone(), observation).is_some() {
                return Err(invariant("duplicate mirrored observation event ID"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        if after.is_some_and(|current| next <= current) {
            return Err(invariant("observation scan cursor did not advance"));
        }
        after = Some(next);
    }
    validate_raw_count(
        repository,
        OBSERVATION_CURSOR_NS,
        observations.len(),
        page_size,
    )
    .await?;
    Ok(observations)
}

async fn validate_consumer_checkpoints<S>(
    repository: &PersistentPaymentRepository<S>,
    observations: &BTreeMap<ObservationEventId, MirroredObservation>,
    page_size: usize,
) -> Result<(), DepositError>
where
    S: Storage,
{
    let cursors = observations
        .values()
        .map(|observation| observation.event.cursor)
        .collect::<BTreeSet<_>>();
    let ingestion = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxIngestion)
        .await?;
    let projection = repository
        .consumer_checkpoint(ConsumerCheckpointName::IxProjection)
        .await?;
    for checkpoint in [ingestion, projection] {
        if checkpoint
            .cursor
            .is_some_and(|cursor| !cursors.contains(&cursor))
        {
            return Err(invariant(
                "PS consumer checkpoint references a missing mirrored IX cursor",
            ));
        }
    }
    if projection.cursor > ingestion.cursor {
        return Err(invariant(
            "IX projection checkpoint is ahead of the ingestion checkpoint",
        ));
    }
    let expected_checkpoint_rows = usize::from(ingestion.cursor.is_some())
        .checked_add(usize::from(projection.cursor.is_some()))
        .ok_or_else(|| invariant("PS consumer checkpoint count overflow"))?;
    let checkpoint_rows = namespace_count(repository, CONSUMER_CHECKPOINT_NS, page_size).await?;
    if checkpoint_rows != expected_checkpoint_rows {
        return Err(invariant(
            "PS consumer checkpoint rows do not match the two known consumer identities",
        ));
    }
    Ok(())
}

async fn load_ledgers<S>(
    repository: &PersistentPaymentRepository<S>,
    deposits: &BTreeMap<DepositId, Deposit>,
    observations: &BTreeMap<ObservationEventId, MirroredObservation>,
    page_size: usize,
) -> Result<LedgerAudit, DepositError>
where
    S: Storage,
{
    let mut entries = BTreeMap::new();
    let mut reconciliation_case_ids = BTreeSet::new();
    let mut deposit_observations = Vec::new();
    let mut projection_ids = BTreeSet::new();
    let mut accounting_count = 0_usize;
    let mut heads = 0_usize;

    for deposit_id in deposits.keys() {
        let mut after = None;
        let mut deposit_entries = BTreeMap::new();
        loop {
            let page = repository
                .entries(LedgerPageRequest {
                    deposit_id: deposit_id.clone(),
                    after: after.clone(),
                    limit: page_size,
                })
                .await?;
            for entry in page.entries {
                if &entry.deposit_id != deposit_id {
                    return Err(invariant(
                        "ledger row is stored under a different deposit prefix",
                    ));
                }
                if deposit_entries.insert(entry.id.clone(), entry).is_some() {
                    return Err(invariant("duplicate ledger entry ID for deposit"));
                }
            }
            let Some(next) = page.next else {
                break;
            };
            ensure_cursor_advanced(after.as_ref(), &next, "ledger")?;
            after = Some(next);
        }

        let head = repository
            .current(deposit_id)
            .await?
            .ok_or_else(|| invariant("deposit has no zero-balance ledger/head"))?;
        heads = heads
            .checked_add(1)
            .ok_or_else(|| invariant("ledger head count overflow"))?;
        if deposit_entries.get(&head.id) != Some(&head) {
            return Err(invariant(
                "ledger head references a missing or different ledger row",
            ));
        }
        let mut reachable = BTreeSet::new();
        let mut cursor = Some(head.id.clone());
        while let Some(entry_id) = cursor {
            if !reachable.insert(entry_id.clone()) {
                return Err(invariant("ledger previous chain contains a cycle"));
            }
            let entry = deposit_entries
                .get(&entry_id)
                .ok_or_else(|| invariant("ledger previous chain contains a missing row"))?;
            cursor = entry.previous.clone();
        }
        if reachable.len() != deposit_entries.len() {
            return Err(invariant(
                "ledger contains rows that are disconnected from its durable head",
            ));
        }
        let root = deposit_entries
            .values()
            .find(|entry| entry.previous.is_none())
            .ok_or_else(|| invariant("ledger has no root row"))?;
        if !matches!(&root.cause, LedgerEntryCause::Opened { .. }) {
            return Err(invariant(
                "ledger root row is not the zero-balance open row",
            ));
        }

        for entry in deposit_entries.into_values() {
            match &entry.cause {
                LedgerEntryCause::Opened { .. } => {}
                LedgerEntryCause::Observation {
                    projection_id,
                    event_id,
                    observation_revision,
                    status,
                    movement_ids,
                    network_fee,
                    ..
                } => {
                    let observation = observations.get(event_id).ok_or_else(|| {
                        invariant("ledger observation row references a missing IX mirror event")
                    })?;
                    if observation.event.transaction.revision != *observation_revision
                        || observation.event.transaction.status != *status
                    {
                        return Err(invariant(
                            "ledger observation revision/status differs from its IX mirror event",
                        ));
                    }
                    let available_movements = observation
                        .event
                        .transaction
                        .movements
                        .iter()
                        .map(|movement| movement.id.clone())
                        .collect::<BTreeSet<_>>();
                    if movement_ids
                        .iter()
                        .any(|movement_id| !available_movements.contains(movement_id))
                    {
                        return Err(invariant(
                            "ledger observation references a missing IX movement",
                        ));
                    }
                    let expected_projection =
                        ProjectionId::for_observation(event_id, *observation_revision, deposit_id);
                    if projection_id != &expected_projection
                        || !projection_ids.insert(projection_id.clone())
                    {
                        return Err(invariant(
                            "ledger projection identity is invalid or duplicated",
                        ));
                    }
                    if let Some(network_fee) = network_fee
                        && observation
                            .event
                            .transaction
                            .fee
                            .as_ref()
                            .is_none_or(|fee| &fee.amount != network_fee)
                    {
                        return Err(invariant(
                            "ledger network fee differs from its mirrored IX fact",
                        ));
                    }
                    deposit_observations.push((
                        deposit_id.clone(),
                        observation.event.cursor,
                        event_id.clone(),
                    ));
                }
                LedgerEntryCause::Accounting { .. } => {
                    accounting_count = accounting_count
                        .checked_add(1)
                        .ok_or_else(|| invariant("accounting row count overflow"))?;
                }
                LedgerEntryCause::ReconciliationResolution { case_id, .. } => {
                    reconciliation_case_ids.insert(case_id.clone());
                }
            }
            let key = (deposit_id.clone(), entry.id.clone());
            if entries.insert(key, entry).is_some() {
                return Err(invariant("duplicate ledger entry key"));
            }
        }
    }

    validate_raw_count(repository, LEDGER_HEAD_NS, heads, page_size).await?;
    validate_raw_count(repository, PROJECTION_NS, projection_ids.len(), page_size).await?;
    validate_raw_count(
        repository,
        ACCOUNTING_IDEMPOTENCY_NS,
        accounting_count,
        page_size,
    )
    .await?;
    Ok(LedgerAudit {
        entries,
        reconciliation_case_ids,
        deposit_observations,
    })
}

async fn load_reconciliation<S>(
    repository: &PersistentPaymentRepository<S>,
    deposits: &BTreeMap<DepositId, Deposit>,
    observations: &BTreeMap<ObservationEventId, MirroredObservation>,
    ledger: &LedgerAudit,
    page_size: usize,
) -> Result<ReconciliationAudit, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut cases = BTreeMap::new();
    let mut typed_resolution_count = 0_usize;
    loop {
        let page = repository
            .cases(ReconciliationPageRequest {
                deposit_id: None,
                after: after.clone(),
                limit: page_size,
                open_only: false,
            })
            .await?;
        for case in page.cases {
            if !deposits.contains_key(&case.deposit_id) {
                return Err(invariant(
                    "reconciliation case references a missing deposit",
                ));
            }
            if !observations.contains_key(&case.triggering_event_id) {
                return Err(invariant(
                    "reconciliation case references a missing IX mirror event",
                ));
            }
            let direct = repository
                .case(&case.id)
                .await?
                .ok_or_else(|| invariant("reconciliation case is not addressable by ID"))?;
            if direct != case {
                return Err(invariant(
                    "reconciliation case ID points to a different case",
                ));
            }
            if let ReconciliationState::Resolved { resolution, .. } = &case.state {
                typed_resolution_count = typed_resolution_count
                    .checked_add(1)
                    .ok_or_else(|| invariant("reconciliation resolution count overflow"))?;
                if resolution.command.operation != CommandOperation::ResolveReconciliation {
                    return Err(invariant(
                        "reconciliation resolution has the wrong command operation",
                    ));
                }
                match &resolution.decision {
                    ReconciliationDecision::ReverseCredit { .. } => {
                        let entry_id = resolution.ledger_entry_id.as_ref().ok_or_else(|| {
                            invariant("reverse-credit resolution has no ledger row")
                        })?;
                        let entry = ledger
                            .entries
                            .get(&(case.deposit_id.clone(), entry_id.clone()))
                            .ok_or_else(|| {
                                invariant(
                                    "reverse-credit resolution references a missing ledger row",
                                )
                            })?;
                        if !matches!(
                            &entry.cause,
                            LedgerEntryCause::ReconciliationResolution { case_id, .. }
                                if case_id == &case.id
                        ) {
                            return Err(invariant(
                                "reverse-credit resolution ledger row references another case",
                            ));
                        }
                    }
                    ReconciliationDecision::AcceptLiability { .. }
                    | ReconciliationDecision::ExternalDebtRecorded { .. } => {
                        if resolution.ledger_entry_id.is_some() {
                            return Err(invariant(
                                "non-ledger reconciliation resolution stores a ledger row",
                            ));
                        }
                    }
                }
            }
            if cases.insert(case.id.clone(), case).is_some() {
                return Err(invariant("duplicate reconciliation case ID"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "reconciliation")?;
        after = Some(next);
    }

    validate_raw_count(
        repository,
        RECONCILIATION_DEPOSIT_NS,
        cases.len(),
        page_size,
    )
    .await?;
    validate_raw_count(
        repository,
        RECONCILIATION_RESOLUTION_IDEMPOTENCY_NS,
        typed_resolution_count,
        page_size,
    )
    .await?;
    for deposit_id in deposits.keys() {
        let expected = cases
            .values()
            .filter(|case| &case.deposit_id == deposit_id)
            .map(|case| case.id.clone())
            .collect::<BTreeSet<_>>();
        let actual = load_cases_for_deposit(repository, deposit_id, page_size).await?;
        if actual != expected {
            return Err(invariant(
                "reconciliation deposit index does not match authoritative cases",
            ));
        }
    }
    Ok(ReconciliationAudit { cases })
}

async fn load_cases_for_deposit<S>(
    repository: &PersistentPaymentRepository<S>,
    deposit_id: &DepositId,
    page_size: usize,
) -> Result<BTreeSet<ReconciliationCaseId>, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut cases = BTreeSet::new();
    loop {
        let page = repository
            .cases(ReconciliationPageRequest {
                deposit_id: Some(deposit_id.clone()),
                after: after.clone(),
                limit: page_size,
                open_only: false,
            })
            .await?;
        for case in page.cases {
            if !cases.insert(case.id) {
                return Err(invariant("duplicate reconciliation case in deposit index"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "reconciliation deposit")?;
        after = Some(next);
    }
    Ok(cases)
}

async fn load_jobs<S>(
    repository: &PersistentPaymentRepository<S>,
    scope: &IndexScope,
    users: &BTreeMap<UserId, User>,
    deposits: &BTreeMap<DepositId, Deposit>,
    page_size: usize,
) -> Result<BTreeMap<JobId, Job>, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut jobs = BTreeMap::new();
    let mut command_keys = BTreeSet::new();
    loop {
        let page = repository
            .jobs(JobPageRequest {
                after: after.clone(),
                limit: page_size,
            })
            .await?;
        for job in page.jobs {
            if job.kind != job.payload.kind()
                || job.resource != job.payload.resource()
                || job.user_id != *job.payload.user_id()
                || job.command.operation != job.payload.operation()
            {
                return Err(invariant(
                    "durable job fields disagree with its typed payload",
                ));
            }
            let user = users
                .get(&job.user_id)
                .ok_or_else(|| invariant("job references a missing user"))?;
            if user.owner != job.user_owner {
                return Err(invariant("job user owner differs from the user record"));
            }
            if job.policy.version.trim().is_empty() {
                return Err(invariant("job policy version is empty"));
            }
            if let JobPayload::CreateDeposit(payload) = &job.payload {
                if &payload.scope != scope {
                    return Err(conflict(
                        "create-deposit job belongs to another chain/network scope",
                    ));
                }
                ensure_asset_scope(&payload.asset, scope, "create-deposit job asset")?;
            }
            let direct = repository
                .job(&job.id)
                .await?
                .ok_or_else(|| invariant("job row is not addressable by ID"))?;
            if direct != job {
                return Err(invariant("job ID points to a different job"));
            }
            let command_key = (
                job.command.principal.clone(),
                job.command.operation,
                job.command.client_key.clone(),
            );
            if !command_keys.insert(command_key) {
                return Err(invariant(
                    "multiple jobs share one scoped idempotency identity",
                ));
            }
            if jobs.insert(job.id.clone(), job).is_some() {
                return Err(invariant("duplicate job ID"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "job")?;
        after = Some(next);
    }

    let ready = jobs
        .values()
        .filter(|job| {
            matches!(
                &job.state,
                JobState::Queued | JobState::Running { .. } | JobState::WaitingRetry { .. }
            )
        })
        .count();
    validate_raw_count(repository, COMMAND_JOB_NS, jobs.len(), page_size).await?;
    validate_raw_count(repository, USER_JOB_NS, jobs.len(), page_size).await?;
    validate_raw_count(repository, RESOURCE_JOB_NS, jobs.len(), page_size).await?;
    validate_raw_count(repository, READY_JOB_NS, ready, page_size).await?;
    let jobs_to_validate = jobs.values().cloned().collect::<Vec<_>>();
    repository
        .validate_migration_job_indexes(&jobs_to_validate)
        .await?;

    for user_id in users.keys() {
        let expected = jobs
            .values()
            .filter(|job| &job.user_id == user_id)
            .map(|job| job.id.clone())
            .collect::<BTreeSet<_>>();
        let actual = load_jobs_for_user(repository, user_id, page_size).await?;
        if actual != expected {
            return Err(invariant(
                "job user index does not match authoritative job rows",
            ));
        }
    }
    let resources = jobs
        .values()
        .map(|job| job.resource.clone())
        .collect::<BTreeSet<_>>();
    for resource in resources {
        let expected = jobs
            .values()
            .filter(|job| job.resource == resource)
            .map(|job| job.id.clone())
            .collect::<BTreeSet<_>>();
        let actual = load_jobs_for_resource(repository, &resource, page_size).await?;
        if actual != expected {
            return Err(invariant(
                "job resource index does not match authoritative job rows",
            ));
        }
    }

    for job in jobs.values() {
        match &job.payload {
            JobPayload::CreateDeposit(_) => {}
            JobPayload::CloseDeposit(payload) => {
                if !deposits.contains_key(&payload.deposit_id) {
                    return Err(invariant("close-deposit job references a missing deposit"));
                }
            }
            JobPayload::CreateCollection(payload) => {
                if !deposits.contains_key(&payload.deposit_id) {
                    return Err(invariant("collection job references a missing deposit"));
                }
            }
            JobPayload::RetryCollection(payload) => {
                if !deposits.contains_key(&payload.deposit_id) {
                    return Err(invariant("collection job references a missing deposit"));
                }
            }
        }
    }
    Ok(jobs)
}

async fn load_jobs_for_user<S>(
    repository: &PersistentPaymentRepository<S>,
    user_id: &UserId,
    page_size: usize,
) -> Result<BTreeSet<JobId>, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut jobs = BTreeSet::new();
    loop {
        let page = repository
            .jobs_for_user(
                user_id,
                JobPageRequest {
                    after: after.clone(),
                    limit: page_size,
                },
            )
            .await?;
        for job in page.jobs {
            if !jobs.insert(job.id) {
                return Err(invariant("duplicate job in user index"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "job user")?;
        after = Some(next);
    }
    Ok(jobs)
}

async fn load_jobs_for_resource<S>(
    repository: &PersistentPaymentRepository<S>,
    resource: &JobResource,
    page_size: usize,
) -> Result<BTreeSet<JobId>, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut jobs = BTreeSet::new();
    loop {
        let page = repository
            .jobs_for_resource(
                resource,
                JobPageRequest {
                    after: after.clone(),
                    limit: page_size,
                },
            )
            .await?;
        for job in page.jobs {
            if !jobs.insert(job.id) {
                return Err(invariant("duplicate job in resource index"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "job resource")?;
        after = Some(next);
    }
    Ok(jobs)
}

async fn load_collections<S>(
    repository: &PersistentPaymentRepository<S>,
    scope: &IndexScope,
    users: &BTreeMap<UserId, User>,
    deposits: &BTreeMap<DepositId, Deposit>,
    jobs: &BTreeMap<JobId, Job>,
    page_size: usize,
) -> Result<BTreeMap<CollectionId, Collection>, DepositError>
where
    S: Storage,
{
    let mut collections = BTreeMap::new();
    let mut active_reservations = 0_usize;
    let mut transaction_legs = 0_usize;
    let mut signed_envelopes = 0_usize;
    for deposit_id in deposits.keys() {
        let mut after = None;
        loop {
            let page = repository
                .collections_for_deposit(
                    deposit_id,
                    CollectionPageRequest {
                        after: after.clone(),
                        limit: page_size,
                    },
                )
                .await?;
            for collection in page.collections {
                if &collection.deposit_id != deposit_id {
                    return Err(invariant(
                        "collection deposit index points to another deposit",
                    ));
                }
                validate_collection(
                    repository,
                    &collection,
                    scope,
                    users,
                    deposits,
                    jobs,
                    &mut active_reservations,
                    &mut transaction_legs,
                    &mut signed_envelopes,
                )
                .await?;
                if collections
                    .insert(collection.id.clone(), collection)
                    .is_some()
                {
                    return Err(invariant("duplicate collection ID or deposit index"));
                }
            }
            let Some(next) = page.next else {
                break;
            };
            ensure_cursor_advanced(after.as_ref(), &next, "collection")?;
            after = Some(next);
        }
    }
    validate_raw_count(repository, COLLECTION_JOB_NS, collections.len(), page_size).await?;
    validate_raw_count(
        repository,
        DEPOSIT_COLLECTION_NS,
        collections.len(),
        page_size,
    )
    .await?;
    validate_raw_count(
        repository,
        ACTIVE_RESERVATION_NS,
        active_reservations,
        page_size,
    )
    .await?;
    validate_raw_count(
        repository,
        COLLECTION_TRANSACTION_NS,
        transaction_legs,
        page_size,
    )
    .await?;
    validate_raw_count(repository, SIGNED_ENVELOPE_NS, signed_envelopes, page_size).await?;
    Ok(collections)
}

#[allow(clippy::too_many_arguments)]
async fn validate_collection<S>(
    repository: &PersistentPaymentRepository<S>,
    collection: &Collection,
    scope: &IndexScope,
    users: &BTreeMap<UserId, User>,
    deposits: &BTreeMap<DepositId, Deposit>,
    jobs: &BTreeMap<JobId, Job>,
    active_reservations: &mut usize,
    transaction_legs: &mut usize,
    signed_envelopes: &mut usize,
) -> Result<(), DepositError>
where
    S: Storage,
{
    ensure_asset_scope(&collection.asset, scope, "collection asset")?;
    ensure_address_scope(&collection.destination, scope, "collection destination")?;
    ensure_asset_scope(
        &collection.reservation.asset,
        scope,
        "collection reservation asset",
    )?;
    if collection.reservation.deposit_id != collection.deposit_id
        || collection.reservation.asset != collection.asset
    {
        return Err(invariant(
            "collection reservation does not match its aggregate",
        ));
    }
    let deposit = deposits
        .get(&collection.deposit_id)
        .ok_or_else(|| invariant("collection references a missing deposit"))?;
    if collection.user_id != deposit.user_id || collection.asset != deposit.asset {
        return Err(invariant(
            "collection user/asset does not match its deposit",
        ));
    }
    if !users.contains_key(&collection.user_id) {
        return Err(invariant("collection references a missing user"));
    }
    let job = jobs
        .get(&collection.job_id)
        .ok_or_else(|| invariant("collection references a missing durable job"))?;
    if job.resource != JobResource::Collection(collection.id.clone())
        || job.user_id != collection.user_id
    {
        return Err(invariant(
            "collection durable job association does not match the aggregate",
        ));
    }
    if collection.policy.version.trim().is_empty() {
        return Err(invariant("collection policy version is empty"));
    }
    if collection.reservation.state == CollectionReservationState::Active {
        *active_reservations = active_reservations
            .checked_add(1)
            .ok_or_else(|| invariant("active reservation count overflow"))?;
    }
    let direct = repository
        .collection(&collection.id)
        .await?
        .ok_or_else(|| invariant("collection row is not addressable by ID"))?;
    if &direct != collection {
        return Err(invariant("collection ID points to a different aggregate"));
    }
    repository
        .validate_migration_collection_indexes(collection)
        .await?;

    let mut leg_ids = BTreeSet::<CollectionLegId>::new();
    for (position, leg) in collection.legs.iter().enumerate() {
        if usize::from(leg.position) != position || !leg_ids.insert(leg.id.clone()) {
            return Err(invariant(
                "collection leg positions or identifiers are invalid",
            ));
        }
        if let Some(allocation) = &leg.allocation {
            if allocation.deposit_id != collection.deposit_id {
                return Err(invariant(
                    "collection allocation references another deposit",
                ));
            }
            ensure_asset_scope(&allocation.asset, scope, "collection allocation asset")?;
            ensure_asset_scope(
                &allocation.allocated_fee_asset,
                scope,
                "collection allocated-fee asset",
            )?;
        }
        if let Some(transaction_id) = leg.state.transaction_id() {
            ensure_transaction_scope(transaction_id, scope, "collection transaction")?;
            let reference = repository
                .leg_for_transaction(transaction_id)
                .await?
                .ok_or_else(|| invariant("collection transaction index is missing"))?;
            if reference
                != (CollectionLegReference {
                    collection_id: collection.id.clone(),
                    leg_id: leg.id.clone(),
                })
            {
                return Err(invariant(
                    "collection transaction index points to another leg",
                ));
            }
            *transaction_legs = transaction_legs
                .checked_add(1)
                .ok_or_else(|| invariant("collection transaction count overflow"))?;
        }
        if let Some(envelope) = repository.signed_envelope(&collection.id, &leg.id).await? {
            if leg.state.transaction_id() != Some(&envelope.expected_transaction_id) {
                return Err(invariant(
                    "signed collection envelope transaction differs from the leg state",
                ));
            }
            *signed_envelopes = signed_envelopes
                .checked_add(1)
                .ok_or_else(|| invariant("signed envelope count overflow"))?;
        }
    }
    Ok(())
}

async fn rebuild_deposit_indexes<S>(
    repository: &PersistentPaymentRepository<S>,
    page_size: usize,
) -> Result<usize, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut rebuilt = 0_usize;
    loop {
        let page = repository
            .rebuild_deposit_indexes(DepositIndexRebuildRequest {
                after: after.clone(),
                limit: page_size,
            })
            .await?;
        rebuilt = rebuilt
            .checked_add(page.scanned)
            .ok_or_else(|| invariant("deposit index rebuild count overflow"))?;
        if page.complete {
            if page.next.is_some() {
                return Err(invariant(
                    "completed deposit index rebuild returned another cursor",
                ));
            }
            break;
        }
        let next = page
            .next
            .ok_or_else(|| invariant("incomplete deposit index rebuild has no cursor"))?;
        ensure_cursor_advanced(after.as_ref(), &next, "deposit index rebuild")?;
        after = Some(next);
    }
    Ok(rebuilt)
}

async fn validate_rebuilt_deposit_indexes<S>(
    repository: &PersistentPaymentRepository<S>,
    deposits: &BTreeMap<DepositId, Deposit>,
    page_size: usize,
) -> Result<(), DepositError>
where
    S: Storage,
{
    validate_raw_count(repository, USER_DEPOSIT_NS, deposits.len(), page_size).await?;
    validate_raw_count(repository, DEPOSIT_STATE_NS, deposits.len(), page_size).await?;
    validate_raw_count(repository, USER_DEPOSIT_STATE_NS, deposits.len(), page_size).await?;
    validate_raw_count(repository, DEPOSIT_INDEX_METADATA_NS, 1, page_size).await?;

    let users = deposits
        .values()
        .map(|deposit| deposit.user_id.clone())
        .collect::<BTreeSet<_>>();
    for user_id in users {
        let expected = deposits
            .values()
            .filter(|deposit| deposit.user_id == user_id)
            .map(|deposit| deposit.id.clone())
            .collect::<BTreeSet<_>>();
        let actual = load_filtered_deposits(repository, Some(user_id), None, page_size).await?;
        if actual != expected {
            return Err(invariant(
                "rebuilt user/deposit index differs from authoritative deposits",
            ));
        }
    }
    for state in [
        DepositStateKind::AwaitingWatch,
        DepositStateKind::Active,
        DepositStateKind::Expired,
        DepositStateKind::Closed,
    ] {
        let expected = deposits
            .values()
            .filter(|deposit| deposit.state.kind() == state)
            .map(|deposit| deposit.id.clone())
            .collect::<BTreeSet<_>>();
        let actual = load_filtered_deposits(repository, None, Some(state), page_size).await?;
        if actual != expected {
            return Err(invariant(
                "rebuilt lifecycle/deposit index differs from authoritative deposits",
            ));
        }
    }
    Ok(())
}

async fn load_filtered_deposits<S>(
    repository: &PersistentPaymentRepository<S>,
    user_id: Option<UserId>,
    state: Option<DepositStateKind>,
    page_size: usize,
) -> Result<BTreeSet<DepositId>, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut deposits = BTreeSet::new();
    loop {
        let page = repository
            .deposits(DepositPageRequest {
                after: after.clone(),
                limit: page_size,
                user_id: user_id.clone(),
                state,
            })
            .await?;
        for deposit in page.deposits {
            if !deposits.insert(deposit.id) {
                return Err(invariant("duplicate deposit in rebuilt index"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        ensure_cursor_advanced(after.as_ref(), &next, "filtered deposit")?;
        after = Some(next);
    }
    Ok(deposits)
}

async fn validate_raw_count<S>(
    repository: &PersistentPaymentRepository<S>,
    namespace: &str,
    expected: usize,
    page_size: usize,
) -> Result<(), DepositError>
where
    S: Storage,
{
    let actual = namespace_count(repository, namespace, page_size).await?;
    if actual != expected {
        return Err(invariant(format!(
            "namespace `{namespace}` has {actual} rows but semantic validation found {expected}"
        )));
    }
    Ok(())
}

async fn namespace_count<S>(
    repository: &PersistentPaymentRepository<S>,
    namespace: &str,
    page_size: usize,
) -> Result<usize, DepositError>
where
    S: Storage,
{
    let mut after = None;
    let mut count = 0_usize;
    loop {
        let page = repository
            .storage()
            .scan(ScanRequest {
                namespace: Namespace(namespace.to_owned()),
                prefix: Vec::new(),
                after: after.clone(),
                limit: page_size,
            })
            .await
            .map_err(|error| DepositError {
                kind: match error.kind {
                    StorageErrorKind::Conflict => DepositErrorKind::Conflict,
                    StorageErrorKind::CorruptData | StorageErrorKind::InvalidRequest => {
                        DepositErrorKind::InvariantViolation
                    }
                    StorageErrorKind::Unavailable | StorageErrorKind::Other => {
                        DepositErrorKind::Storage
                    }
                },
                message: error.message,
            })?;
        count = count
            .checked_add(page.entries.len())
            .ok_or_else(|| invariant(format!("namespace `{namespace}` row count overflow")))?;
        let Some(next) = page.next else {
            break;
        };
        if Some(&next) == after.as_ref() {
            return Err(invariant(format!(
                "namespace `{namespace}` scan cursor did not advance"
            )));
        }
        after = Some(next);
    }
    Ok(count)
}

fn ensure_asset_scope(
    asset: &AssetId,
    scope: &IndexScope,
    field: &str,
) -> Result<(), DepositError> {
    ensure_chain_scope(&asset.chain, scope, field)
}

fn ensure_address_scope(
    address: &CanonicalAddress,
    scope: &IndexScope,
    field: &str,
) -> Result<(), DepositError> {
    ensure_chain_scope(&address.chain, scope, field)
}

fn ensure_transaction_scope(
    transaction_id: &CanonicalTransactionId,
    scope: &IndexScope,
    field: &str,
) -> Result<(), DepositError> {
    ensure_chain_scope(&transaction_id.chain, scope, field)
}

fn ensure_chain_scope(
    chain: &ChainId,
    scope: &IndexScope,
    field: &str,
) -> Result<(), DepositError> {
    if chain != &scope.chain {
        return Err(conflict(format!(
            "{field} belongs to chain `{}`, not operator-provided `{}`",
            chain.0, scope.chain.0
        )));
    }
    Ok(())
}

fn ensure_cursor_advanced<T: Ord>(
    previous: Option<&T>,
    next: &T,
    name: &str,
) -> Result<(), DepositError> {
    if previous.is_some_and(|previous| next <= previous) {
        return Err(invariant(format!("{name} scan cursor did not advance")));
    }
    Ok(())
}

fn conflict(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Conflict,
        message: message.into(),
    }
}

fn invariant(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}
