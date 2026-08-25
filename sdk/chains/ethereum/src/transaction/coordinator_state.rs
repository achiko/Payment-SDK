use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
};

use indexing::SourceError;
use tokio::sync::Notify;

use super::{PreparedEntry, chain_error, source_error};
use crate::{Accounts, Address, ChainError, ChainErrorKind, Transactions};
use crate::{SignedTransaction, TransactionId};

pub(super) struct Core {
    pub(super) accounts: Arc<dyn Accounts>,
    pub(super) transactions: Arc<dyn Transactions>,
    state: Mutex<State>,
    pub(super) changed: Notify,
}

impl Core {
    pub(super) fn new(accounts: Arc<dyn Accounts>, transactions: Arc<dyn Transactions>) -> Self {
        Self {
            accounts,
            transactions,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(super) fn admission(self: &Arc<Self>, senders: &[(Address, usize)]) -> Admission {
        let mut state = self.state();
        for (source, index) in senders {
            let Some(sender) = state.senders.get(source) else {
                continue;
            };
            if sender.active.is_some() {
                return Admission::Wait;
            }
            for id in &sender.records {
                let Some(record) = state.records.get(id) else {
                    continue;
                };
                match record.status {
                    RecordStatus::Unknown => {
                        return Admission::Recover {
                            id: id.clone(),
                            index: *index,
                        };
                    }
                    RecordStatus::Prepared | RecordStatus::Reconciling => {
                        return Admission::Wait;
                    }
                }
            }
        }
        let id = state.next_operation;
        let Some(next) = id.checked_add(1) else {
            return Admission::Exhausted(senders.first().map_or(0, |(_, index)| *index));
        };
        state.next_operation = next;
        for (source, _) in senders {
            state.senders.entry(source.clone()).or_default().active = Some(id);
        }
        Admission::Acquired(Operation {
            core: self.clone(),
            id,
        })
    }

    pub(super) fn floor(&self, source: &Address) -> Option<u64> {
        self.state()
            .senders
            .get(source)
            .and_then(|sender| sender.floor)
    }

    pub(super) fn register(
        &self,
        operation: u64,
        entries: &[PreparedEntry],
    ) -> Result<(), ChainError> {
        let mut state = self.state();
        for entry in entries {
            if state.records.contains_key(&entry.signed.id) {
                return Err(chain_error(
                    ChainErrorKind::Other,
                    "Ethereum signed transaction is already coordinated",
                ));
            }
            if state
                .senders
                .get(&entry.source)
                .is_none_or(|sender| sender.active != Some(operation))
            {
                return Err(chain_error(
                    ChainErrorKind::Other,
                    "Ethereum sender lost its atomic coordinator admission",
                ));
            }
        }
        for entry in entries {
            state
                .senders
                .entry(entry.source.clone())
                .or_default()
                .records
                .insert(entry.signed.id.clone());
            state.records.insert(
                entry.signed.id.clone(),
                Record {
                    source: entry.source.clone(),
                    nonce: entry.nonce,
                    transaction: entry.signed.clone(),
                    operation: Some(operation),
                    status: RecordStatus::Prepared,
                },
            );
        }
        Ok(())
    }

    pub(super) fn detach(&self, operation: u64, id: &TransactionId) -> Result<(), ChainError> {
        let mut state = self.state();
        let record = state.records.get_mut(id).ok_or_else(|| {
            chain_error(
                ChainErrorKind::Other,
                "Ethereum prepared transaction is not coordinated",
            )
        })?;
        if record.operation != Some(operation) || record.status != RecordStatus::Prepared {
            return Err(chain_error(
                ChainErrorKind::Other,
                "Ethereum prepared transaction cannot leave its batch admission",
            ));
        }
        record.operation = None;
        Ok(())
    }

    pub(super) fn claim(
        self: &Arc<Self>,
        id: &TransactionId,
        expected: Option<&SignedTransaction>,
    ) -> Result<Claim, SourceError> {
        let mut state = self.state();
        let record = state.records.get_mut(id).ok_or_else(|| {
            source_error(
                "signed Ethereum transaction has no process-local nonce reservation",
                false,
            )
        })?;
        if expected.is_some_and(|transaction| transaction != &record.transaction) {
            return Err(source_error(
                "signed Ethereum transaction differs from its reserved exact envelope",
                false,
            ));
        }
        let recovery = match record.status {
            RecordStatus::Prepared => false,
            RecordStatus::Unknown => true,
            RecordStatus::Reconciling => return Ok(Claim::Wait),
        };
        record.status = RecordStatus::Reconciling;
        Ok(Claim::Ready(SubmissionClaim {
            transaction: record.transaction.clone(),
            recovery,
            guard: Submission {
                core: self.clone(),
                id: id.clone(),
                armed: true,
            },
        }))
    }

    fn finish_operation(&self, operation: u64) {
        let mut state = self.state();
        let cancelled = state
            .records
            .iter()
            .filter(|(_, record)| {
                record.operation == Some(operation) && record.status == RecordStatus::Prepared
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in cancelled {
            state.remove_record(&id);
        }
        for sender in state.senders.values_mut() {
            if sender.active == Some(operation) {
                sender.active = None;
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

#[derive(Default)]
struct State {
    next_operation: u64,
    senders: BTreeMap<Address, SenderState>,
    records: BTreeMap<TransactionId, Record>,
}

impl State {
    fn remove_record(&mut self, id: &TransactionId) -> Option<Record> {
        let record = self.records.remove(id)?;
        if let Some(sender) = self.senders.get_mut(&record.source) {
            sender.records.remove(id);
        }
        Some(record)
    }
}

#[derive(Default)]
struct SenderState {
    active: Option<u64>,
    floor: Option<u64>,
    records: BTreeSet<TransactionId>,
}

struct Record {
    source: Address,
    nonce: u64,
    transaction: SignedTransaction,
    operation: Option<u64>,
    status: RecordStatus,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordStatus {
    Prepared,
    Unknown,
    Reconciling,
}

pub(super) enum Admission {
    Acquired(Operation),
    Wait,
    Recover { id: TransactionId, index: usize },
    Exhausted(usize),
}

pub(super) struct Operation {
    core: Arc<Core>,
    pub(super) id: u64,
}

impl Drop for Operation {
    fn drop(&mut self) {
        self.core.finish_operation(self.id);
    }
}

pub(super) enum Claim {
    Ready(SubmissionClaim),
    Wait,
}

pub(super) struct SubmissionClaim {
    pub(super) transaction: SignedTransaction,
    pub(super) recovery: bool,
    pub(super) guard: Submission,
}

pub(super) struct Submission {
    core: Arc<Core>,
    id: TransactionId,
    armed: bool,
}

impl Submission {
    pub(super) fn accept(mut self) -> Result<TransactionId, SourceError> {
        let mut state = self.core.state();
        let record = state.remove_record(&self.id).ok_or_else(|| {
            source_error(
                "Ethereum transaction disappeared during submission reconciliation",
                true,
            )
        })?;
        let next = record.nonce.checked_add(1).ok_or_else(|| {
            source_error(
                "accepted Ethereum transaction exhausted the nonce range",
                false,
            )
        })?;
        let sender = state.senders.entry(record.source).or_default();
        sender.floor = Some(sender.floor.map_or(next, |floor| floor.max(next)));
        self.armed = false;
        drop(state);
        self.core.changed.notify_waiters();
        Ok(self.id.clone())
    }

    pub(super) fn reject(mut self) {
        self.core.state().remove_record(&self.id);
        self.armed = false;
        self.core.changed.notify_waiters();
    }
}

impl Drop for Submission {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(record) = self.core.state().records.get_mut(&self.id)
            && record.status == RecordStatus::Reconciling
        {
            record.status = RecordStatus::Unknown;
        }
        self.core.changed.notify_waiters();
    }
}
