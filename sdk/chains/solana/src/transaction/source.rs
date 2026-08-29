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
    preparing: Arc<Mutex<BTreeSet<Address>>>,
}

pub(super) struct SourceLeases {
    coordinator: SourceCoordinator,
    sources: Vec<Address>,
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
        let mut preparing = self
            .preparing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let busy_index = transfers
            .iter()
            .filter(|transfer| preparing.contains(transfer.source()))
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
        preparing.extend(sources.iter().cloned());
        drop(preparing);
        Ok(SourceLeases {
            coordinator: self.clone(),
            sources,
        })
    }
}

impl Drop for SourceLeases {
    fn drop(&mut self) {
        let mut preparing = self
            .coordinator
            .preparing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for source in &self.sources {
            preparing.remove(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(index: usize, source: u8) -> ResolvedTransfer {
        ResolvedTransfer::new(index, Address::from_bytes([source; 32]), String::new())
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
}
