use std::{collections::BTreeSet, sync::Arc};

use base::Decimal;
use deposits::{
    BatchJob, BatchParticipant, Collection, CollectionId, CollectionJob, CollectionLegKind,
    CollectionMode, CollectionPlan, Collections, CommandIdentity, CommandOperation,
    CommandPrincipal, CreateBatch, CreateLeg, Deposit, DepositError, DepositErrorKind, DepositId,
    DepositReader, DepositState, IdempotencyKey, JobCommands, JobId, JobPayload, JobPlan,
    LedgerReader, PolicyIdentity, RequestHash, ResourceId, ResourceProof, SpendResource, User,
    UserStore,
};
use indexing::{
    AssetId, CanonicalAddress, IndexScope, OutputCursor, OutputQuery, OutputRequest, OutputSnapshot,
};

const PAGE_SIZE: usize = 1_000;

/// Executable collection policy selected by the application composition root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionPolicy {
    pub identity: PolicyIdentity,
    pub minimum_amount: Decimal,
    pub minimum_confirmations: u64,
    pub coinbase_maturity: u64,
    pub max_participants: usize,
    pub max_inputs: usize,
    pub gas_amount: Option<Decimal>,
}

impl CollectionPolicy {
    pub(crate) fn configured(
        config: &crate::DepositConfig,
    ) -> Result<Self, crate::CompositionError> {
        let digest = hex::decode(&config.policy_digest)
            .map_err(|error| crate::CompositionError::adapter("collection policy digest", error))?;
        let digest: [u8; 32] = digest.try_into().map_err(|_| {
            crate::CompositionError::invalid("collection policy digest must contain 32 bytes")
        })?;
        Ok(Self {
            identity: PolicyIdentity {
                version: config.policy_version.clone(),
                digest,
            },
            minimum_amount: config.minimum_collection.parse().map_err(|error| {
                crate::CompositionError::adapter("minimum collection amount", error)
            })?,
            minimum_confirmations: config.minimum_confirmations,
            coinbase_maturity: config.coinbase_maturity,
            max_participants: config.max_participants,
            max_inputs: config.max_inputs,
            gas_amount: config
                .gas_amount
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|error| {
                    crate::CompositionError::adapter("collection gas amount", error)
                })?,
        })
    }
}

/// Caller identity and stable IDs. All financial and chain evidence is loaded
/// by the planner from durable PS and IX state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRequest {
    pub collection_id: CollectionId,
    pub job_id: JobId,
    pub principal: CommandPrincipal,
    pub idempotency_key: IdempotencyKey,
    pub request_hash: RequestHash,
    pub deposit_ids: Vec<DepositId>,
    pub created_at: u64,
}

pub trait PlanStore:
    DepositReader + LedgerReader + Collections + JobCommands + UserStore + Send + Sync
{
}

impl<T> PlanStore for T where
    T: DepositReader + LedgerReader + Collections + JobCommands + UserStore + Send + Sync
{
}

/// Plans one configured collection kind without accepting caller-provided
/// amounts, assets, destinations, outpoints, or chain evidence.
pub struct Planner {
    store: Arc<dyn PlanStore>,
    outputs: Arc<dyn OutputQuery>,
    scope: IndexScope,
    asset: AssetId,
    destination: CanonicalAddress,
    mode: CollectionMode,
    policy: CollectionPolicy,
}

struct LoadedOutputs {
    snapshot: OutputSnapshot,
    participant: BatchParticipant,
}

impl Planner {
    #[must_use]
    pub fn new(
        store: Arc<dyn PlanStore>,
        outputs: Arc<dyn OutputQuery>,
        scope: IndexScope,
        asset: AssetId,
        destination: CanonicalAddress,
        mode: CollectionMode,
        policy: CollectionPolicy,
    ) -> Self {
        Self {
            store,
            outputs,
            scope,
            asset,
            destination,
            mode,
            policy,
        }
    }

    pub async fn plan(&self, mut request: PlanRequest) -> Result<Collection, DepositError> {
        self.validate_request(&request)?;
        let command = self.command(&request);
        if let Some(job) = self.store.job_for_command(&command).await? {
            if job.id != request.job_id {
                return Err(conflict(
                    "collection request resolved to another durable job",
                ));
            }
            if let Some(collection) = self.store.collection(&request.collection_id).await? {
                return Ok(collection);
            }
        }
        request.deposit_ids.sort();
        let deposits = self.load_deposits(&request.deposit_ids).await?;
        for deposit in &deposits {
            self.store
                .ensure_user(User {
                    id: deposit.user_id.clone(),
                    owner: request.principal.clone(),
                    first_seen_at: request.created_at,
                })
                .await?;
        }
        match self.mode {
            CollectionMode::UtxoBatch => self.plan_utxo(request, deposits).await,
            CollectionMode::AccountTransfer | CollectionMode::TokenWithGas => {
                self.plan_account(request, deposits).await
            }
        }
    }

    fn validate_request(&self, request: &PlanRequest) -> Result<(), DepositError> {
        if request.collection_id.0.trim().is_empty()
            || request.job_id.0.trim().is_empty()
            || request.principal.0.trim().is_empty()
            || request.idempotency_key.0.trim().is_empty()
            || request.deposit_ids.is_empty()
            || request.deposit_ids.len() > self.policy.max_participants
            || self.policy.max_participants == 0
            || self.policy.max_inputs == 0
            || self.policy.minimum_amount <= Decimal::zero()
            || self.asset.chain != self.scope.chain
            || self.destination.scope != self.scope
        {
            return Err(invalid(
                "collection planning configuration or request is invalid",
            ));
        }
        let unique = request.deposit_ids.iter().collect::<BTreeSet<_>>();
        if unique.len() != request.deposit_ids.len() {
            return Err(invalid("collection deposit IDs must be unique"));
        }
        if self.mode != CollectionMode::TokenWithGas && self.policy.gas_amount.is_some() {
            return Err(invalid("gas funding is only valid for token collection"));
        }
        Ok(())
    }

    async fn load_deposits(&self, ids: &[DepositId]) -> Result<Vec<Deposit>, DepositError> {
        let mut deposits = Vec::with_capacity(ids.len());
        for id in ids {
            let deposit = self
                .store
                .deposit(id)
                .await?
                .ok_or_else(|| missing("collection deposit does not exist"))?;
            if !matches!(deposit.state, DepositState::Active { .. })
                || deposit.asset != self.asset
                || deposit.address.scope != self.scope
            {
                return Err(conflict(
                    "collection deposit is not active for the configured asset",
                ));
            }
            deposits.push(deposit);
        }
        deposits.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(deposits)
    }

    async fn plan_utxo(
        &self,
        request: PlanRequest,
        deposits: Vec<Deposit>,
    ) -> Result<Collection, DepositError> {
        let mut participants = Vec::with_capacity(deposits.len());
        let mut snapshot = None;
        let mut resources = BTreeSet::new();
        let mut input_count = 0usize;
        for deposit in deposits {
            let loaded = self.load_outputs(deposit).await?;
            if snapshot
                .as_ref()
                .is_some_and(|value| value != &loaded.snapshot)
            {
                return Err(conflict("canonical output snapshot changed while planning"));
            }
            snapshot.get_or_insert(loaded.snapshot);
            for resource in &loaded.participant.spend_resources {
                if !resources.insert(resource.id.clone()) {
                    return Err(invalid("indexer returned a duplicate spendable output"));
                }
            }
            input_count = input_count
                .checked_add(loaded.participant.spend_resources.len())
                .ok_or_else(|| invalid("collection input count exceeds the supported range"))?;
            if input_count > self.policy.max_inputs {
                return Err(invalid("collection exceeds the configured input limit"));
            }
            participants.push(loaded.participant);
        }
        let payload = JobPayload::CreateBatch(BatchJob {
            collection_id: request.collection_id.clone(),
            deposit_ids: participants
                .iter()
                .map(|value| value.deposit_id.clone())
                .collect(),
        });
        self.create_job(&request, payload).await?;
        self.store
            .create_or_replay_utxo_batch(CreateBatch {
                id: request.collection_id,
                job_id: request.job_id,
                asset: self.asset.clone(),
                destination: self.destination.clone(),
                policy: self.policy.identity.clone(),
                participants,
                leg: CreateLeg {
                    id: deposits::LegId("sweep".to_owned()),
                    kind: CollectionLegKind::Sweep,
                    planned_amount: None,
                },
                created_at: request.created_at,
            })
            .await
            .map(|outcome| outcome.collection().clone())
    }

    async fn load_outputs(&self, deposit: Deposit) -> Result<LoadedOutputs, DepositError> {
        let head = self
            .store
            .current(&deposit.id)
            .await?
            .ok_or_else(|| conflict("collection deposit has no ledger head"))?;
        let mut after: Option<OutputCursor> = None;
        let mut snapshot = None;
        let mut resources = Vec::new();
        let mut amount = Decimal::zero();
        loop {
            let page = self
                .outputs
                .outputs(OutputRequest {
                    scope: self.scope.clone(),
                    address: deposit.address.clone(),
                    after: after.clone(),
                    limit: PAGE_SIZE,
                })
                .await
                .map_err(|error| DepositError {
                    kind: DepositErrorKind::Store,
                    message: format!("cannot read canonical spendable outputs: {error}"),
                })?;
            if page.snapshot.checkpoint.is_none()
                || snapshot
                    .as_ref()
                    .is_some_and(|value| value != &page.snapshot)
            {
                return Err(conflict("canonical output snapshot changed while planning"));
            }
            snapshot.get_or_insert_with(|| page.snapshot.clone());
            let checkpoint = page
                .snapshot
                .checkpoint
                .as_ref()
                .ok_or_else(|| conflict("canonical output snapshot has no checkpoint"))?;
            for output in page.outputs {
                if output.address != deposit.address
                    || output.asset != self.asset
                    || !output.id.transaction.belongs_to(&self.scope)
                    || output.amount <= Decimal::zero()
                    || output.amount.scale() != 0
                {
                    return Err(invalid("indexer returned an invalid spendable output"));
                }
                let confirmations = checkpoint
                    .height
                    .0
                    .checked_sub(output.created_at.0)
                    .and_then(|depth| depth.checked_add(1))
                    .ok_or_else(|| conflict("output is newer than the canonical checkpoint"))?;
                let required = if output.coinbase {
                    self.policy
                        .coinbase_maturity
                        .max(self.policy.minimum_confirmations)
                } else {
                    self.policy.minimum_confirmations
                };
                if confirmations < required {
                    continue;
                }
                amount = amount
                    .checked_add(&output.amount)
                    .map_err(|_| invalid("spendable output amount exceeds the supported range"))?;
                resources.push(SpendResource {
                    id: ResourceId {
                        transaction_id: output.id.transaction,
                        output_index: output.id.index,
                    },
                    amount: output.amount,
                    evidence: ResourceProof::new(output.evidence)?,
                });
            }
            match page.next {
                Some(next) if after.as_ref() != Some(&next) => after = Some(next),
                Some(_) => return Err(conflict("output pagination cursor did not advance")),
                None => break,
            }
        }
        resources.sort_by(|left, right| left.id.cmp(&right.id));
        if resources.is_empty() || amount < self.policy.minimum_amount {
            return Err(conflict(
                "collection deposit has no eligible spendable balance",
            ));
        }
        Ok(LoadedOutputs {
            snapshot: snapshot
                .ok_or_else(|| conflict("indexer returned no canonical output snapshot"))?,
            participant: BatchParticipant {
                user_id: deposit.user_id,
                deposit_id: deposit.id,
                expected_ledger_head: head.id,
                reservation_amount: amount,
                spend_resources: resources,
            },
        })
    }

    async fn plan_account(
        &self,
        request: PlanRequest,
        deposits: Vec<Deposit>,
    ) -> Result<Collection, DepositError> {
        let [deposit] = deposits.as_slice() else {
            return Err(invalid("account collection requires exactly one deposit"));
        };
        let head = self
            .store
            .current(&deposit.id)
            .await?
            .ok_or_else(|| conflict("collection deposit has no ledger head"))?;
        let amount = head.balances.balance;
        if amount.is_zero() || amount < self.policy.minimum_amount {
            return Err(conflict(
                "collection deposit has no eligible spendable balance",
            ));
        }
        let payload = JobPayload::CollectionPlan(CollectionJob {
            collection_id: request.collection_id.clone(),
            deposit_id: deposit.id.clone(),
            user_id: deposit.user_id.clone(),
        });
        self.create_job(&request, payload).await?;
        let mut legs = Vec::new();
        if let Some(gas_amount) = &self.policy.gas_amount {
            if gas_amount <= &Decimal::zero() {
                return Err(invalid("configured gas amount must be positive"));
            }
            legs.push(CreateLeg {
                id: deposits::LegId("gas".to_owned()),
                kind: CollectionLegKind::GasFunding,
                planned_amount: Some(gas_amount.clone()),
            });
        }
        legs.push(CreateLeg {
            id: deposits::LegId("sweep".to_owned()),
            kind: CollectionLegKind::Sweep,
            planned_amount: None,
        });
        self.store
            .create_or_replay_collection(CollectionPlan {
                id: request.collection_id,
                job_id: request.job_id,
                user_id: deposit.user_id.clone(),
                deposit_id: deposit.id.clone(),
                mode: self.mode,
                asset: self.asset.clone(),
                destination: self.destination.clone(),
                policy: self.policy.identity.clone(),
                reservation_amount: amount,
                legs,
                created_at: request.created_at,
            })
            .await
            .map(|outcome| outcome.collection().clone())
    }

    async fn create_job(
        &self,
        request: &PlanRequest,
        payload: JobPayload,
    ) -> Result<(), DepositError> {
        let command = self.command(request);
        self.store
            .create_or_replay(JobPlan {
                id: request.job_id.clone(),
                command,
                payload,
                user_owner: request.principal.clone(),
                policy: self.policy.identity.clone(),
                created_at: request.created_at,
            })
            .await?;
        Ok(())
    }

    fn command(&self, request: &PlanRequest) -> CommandIdentity {
        CommandIdentity {
            principal: request.principal.clone(),
            operation: CommandOperation::CollectionPlan,
            client_key: request.idempotency_key.clone(),
            request_hash: request.request_hash,
        }
    }
}

fn invalid(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::InvariantViolation,
        message: message.into(),
    }
}

fn conflict(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::Conflict,
        message: message.into(),
    }
}

fn missing(message: impl Into<String>) -> DepositError {
    DepositError {
        kind: DepositErrorKind::NotFound,
        message: message.into(),
    }
}
