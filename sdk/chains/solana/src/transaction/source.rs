use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use wallets::{Error, ErrorKind, SendError};

use crate::Address;

use super::ResolvedTransfer;

/// Process-local owner of Solana sources currently being prepared.
#[derive(Clone, Default)]
pub struct SourceCoordinator {
    state: Arc<Mutex<SourceState>>,
}

pub(super) struct SourceLeases {
    coordinator: SourceCoordinator,
    sources: Vec<Address>,
}

pub(super) struct GuardedSources {
    coordinator: SourceCoordinator,
    sources: BTreeSet<Address>,
}

#[derive(Default)]
struct SourceState {
    preparing: BTreeSet<Address>,
    guarded: BTreeSet<Address>,
}

impl SourceCoordinator {
    pub(super) fn lease(
        &self,
        transfers: &[ResolvedTransfer],
        batch: bool,
    ) -> Result<SourceLeases, SendError> {
        let mut earliest = BTreeMap::<Address, usize>::new();
        for transfer in transfers {
            earliest
                .entry(transfer.source().clone())
                .and_modify(|index| *index = (*index).min(transfer.index()))
                .or_insert(transfer.index());
        }
        let sources = earliest.keys().cloned().collect::<Vec<_>>();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let busy_index = transfers
            .iter()
            .filter(|transfer| {
                state.preparing.contains(transfer.source())
                    || state.guarded.contains(transfer.source())
            })
            .map(ResolvedTransfer::index)
            .min();
        if let Some(index) = busy_index {
            let error = Error::new(ErrorKind::SourceBusy, "Solana source is already in use");
            return Err(if batch {
                SendError::item(index, Vec::new(), error)
            } else {
                SendError::operation(ErrorKind::SourceBusy, error.message)
            });
        }
        state.preparing.extend(sources.iter().cloned());
        drop(state);
        Ok(SourceLeases {
            coordinator: self.clone(),
            sources,
        })
    }

    pub(super) fn release_guard(&self, source: &Address) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .guarded
            .remove(source);
    }
}

impl SourceLeases {
    pub(super) fn guard(mut self) -> GuardedSources {
        let sources = self.sources.drain(..).collect::<BTreeSet<_>>();
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for source in &sources {
            state.preparing.remove(source);
            state.guarded.insert(source.clone());
        }
        drop(state);
        GuardedSources {
            coordinator: self.coordinator.clone(),
            sources,
        }
    }
}

impl GuardedSources {
    pub(super) fn retain_ambiguity(mut self, source: &Address) {
        let released = self
            .sources
            .iter()
            .filter(|candidate| *candidate != source)
            .cloned()
            .collect::<Vec<_>>();
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for address in released {
            self.sources.remove(&address);
            state.guarded.remove(&address);
        }
        self.sources.clear();
        drop(state);
    }
}

impl Drop for SourceLeases {
    fn drop(&mut self) {
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for source in &self.sources {
            state.preparing.remove(source);
        }
    }
}

impl Drop for GuardedSources {
    fn drop(&mut self) {
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for source in &self.sources {
            state.guarded.remove(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(index: usize, source: u8) -> ResolvedTransfer {
        ResolvedTransfer::new(
            index,
            Address::from_bytes([source; 32]),
            String::new(),
            crate::Lamport::from_atomic(1),
        )
    }

    #[test]
    fn leases_distinct_sources_canonically_and_reports_earliest_alias() {
        let coordinator = SourceCoordinator::default();
        let held = coordinator
            .lease(&[transfer(0, 9), transfer(1, 1)], true)
            .expect("initial leases");
        assert_eq!(
            held.sources,
            [Address::from_bytes([1; 32]), Address::from_bytes([9; 32])]
        );

        let failure = coordinator
            .lease(
                &[
                    transfer(4, 1),
                    transfer(1, 9),
                    transfer(3, 2),
                    transfer(2, 9),
                ],
                true,
            )
            .err()
            .expect("busy alias");
        assert_eq!(failure.failed_index, Some(1));
        assert_eq!(failure.source.kind, ErrorKind::SourceBusy);
        assert!(failure.accepted.is_empty());
        assert!(failure.ambiguous_transaction_id.is_none());

        drop(held);
        coordinator
            .lease(&[transfer(0, 9), transfer(1, 1)], true)
            .expect("drop releases all leases");
    }

    #[test]
    fn single_source_busy_has_no_item_index() {
        let coordinator = SourceCoordinator::default();
        let _held = coordinator.lease(&[transfer(0, 7)], false).unwrap();
        let failure = coordinator
            .lease(&[transfer(0, 7)], false)
            .err()
            .expect("busy source");
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.source.kind, ErrorKind::SourceBusy);
    }

    #[test]
    fn transition_to_guarded_is_atomic_and_retains_only_ambiguous_source() {
        let coordinator = SourceCoordinator::default();
        let leases = coordinator
            .lease(&[transfer(0, 2), transfer(1, 1)], true)
            .expect("preparing sources");
        let guarded = leases.guard();

        let busy = coordinator
            .lease(&[transfer(0, 1)], false)
            .err()
            .expect("guarded source remains busy");
        assert_eq!(busy.source.kind, ErrorKind::SourceBusy);

        guarded.retain_ambiguity(&Address::from_bytes([1; 32]));
        coordinator
            .lease(&[transfer(0, 2)], false)
            .expect("released unattempted source");
        assert!(coordinator.lease(&[transfer(0, 1)], false).is_err());

        coordinator.release_guard(&Address::from_bytes([1; 32]));
        coordinator
            .lease(&[transfer(0, 1)], false)
            .expect("dropping guard releases ambiguity barrier");
    }

    #[test]
    fn a_fresh_process_coordinator_cannot_recover_an_ambiguous_guard() {
        let running = SourceCoordinator::default();
        running
            .lease(&[transfer(0, 7)], false)
            .expect("lease")
            .guard()
            .retain_ambiguity(&Address::from_bytes([7; 32]));
        assert!(running.lease(&[transfer(0, 7)], false).is_err());

        SourceCoordinator::default()
            .lease(&[transfer(0, 7)], false)
            .expect("fresh process has no durable ambiguity state");
    }
}
