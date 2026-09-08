use std::{error, fmt, future::Future, pin::Pin, sync::Arc};

use base::{Decimal, Id};

use crate::{AddressText, Error, ErrorKind, Wallet};

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Id>, SendError>> + Send + 'a>>;

/// One requested value transfer from an already-created wallet.
pub struct Transfer {
    pub wallet: Arc<dyn Wallet>,
    pub to: AddressText,
    pub amount: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendError {
    /// Transactions definitely acknowledged before this batch stopped.
    pub accepted: Vec<Id>,
    /// Original authored item index, present only for an item-scoped failure.
    pub failed_index: Option<usize>,
    /// Canonical local ID moved from an ambiguous concrete transaction error.
    pub ambiguous_transaction_id: Option<Id>,
    pub source: Error,
}

impl SendError {
    /// Creates a batch-collection failure before any item can be selected.
    #[must_use]
    pub fn collection(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::definite(kind, message)
    }

    /// Creates an operation-wide failure that belongs to no individual item.
    #[must_use]
    pub fn operation(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::definite(kind, message)
    }

    /// Creates a failure for one original item in an ordered batch.
    #[must_use]
    pub fn item(failed_index: usize, accepted: Vec<Id>, source: Error) -> Self {
        Self::new(accepted, Some(failed_index), source)
    }

    /// Creates an index-free failure for one transaction grouping several items.
    #[must_use]
    pub fn grouped(accepted: Vec<Id>, source: Error) -> Self {
        Self::new(accepted, None, source)
    }

    fn definite(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            accepted: Vec::new(),
            failed_index: None,
            ambiguous_transaction_id: None,
            source: Error::new(kind, message),
        }
    }

    fn new(accepted: Vec<Id>, failed_index: Option<usize>, mut source: Error) -> Self {
        let ambiguous_transaction_id = source.ambiguous_transaction_id.take();
        Self {
            accepted,
            failed_index,
            ambiguous_transaction_id,
            source,
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failed_index {
            Some(failed_index) => write!(
                formatter,
                "transaction {failed_index} failed after {} accepted transaction(s): {}",
                self.accepted.len(),
                self.source
            ),
            None => self.source.fmt(formatter),
        }
    }
}

impl error::Error for SendError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Sends a batch using the concrete chain's native transaction model.
///
/// UTXO chains may combine compatible transfers into one transaction. Account
/// chains normally submit one nonce-ordered transaction per transfer.
/// Callers route batches through [`crate::Wallets::send_all`], which proves
/// that every wallet belongs to the registered family owning this sender.
/// Constructing [`Transfer`] values from another provider or family is outside
/// this capability's contract.
pub trait Sender: Send + Sync {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a>;
}

impl<T: Sender + ?Sized> Sender for Arc<T> {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        (**self).send(transfers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_failure_has_no_transaction_metadata_or_indexed_display() {
        let failure =
            SendError::collection(ErrorKind::InvalidBatch, "at least one transfer is required");

        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.source.kind, ErrorKind::InvalidBatch);
        assert_eq!(failure.to_string(), "at least one transfer is required");
        assert!(!failure.to_string().contains("transaction 0"));
    }

    #[test]
    fn operation_failure_has_no_transaction_metadata_or_indexed_display() {
        let failure = SendError::operation(ErrorKind::Unavailable, "account acquisition failed");

        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.to_string(), "account acquisition failed");
    }

    #[test]
    fn item_failure_preserves_original_index_and_acknowledged_prefix() {
        let accepted = vec![Id::new("first"), Id::new("second")];
        let ambiguous = Id::new("item-canonical-id");
        let failure = SendError::item(
            4,
            accepted.clone(),
            Error::from(
                base::TransactionError::new(
                    base::TransactionErrorKind::Timeout,
                    "item submission outcome is unknown",
                )
                .with_ambiguous_transaction_id(ambiguous.clone()),
            ),
        );

        assert_eq!(failure.accepted, accepted);
        assert_eq!(failure.failed_index, Some(4));
        assert_eq!(failure.ambiguous_transaction_id, Some(ambiguous));
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(
            failure.to_string(),
            "transaction 4 failed after 2 accepted transaction(s): item submission outcome is unknown"
        );
    }

    #[test]
    fn grouped_failure_moves_ambiguity_without_inventing_an_item_index() {
        let ambiguous = Id::new("grouped-canonical-id");
        let source = Error::from(
            base::TransactionError::new(
                base::TransactionErrorKind::Timeout,
                "grouped submission outcome is unknown",
            )
            .with_ambiguous_transaction_id(ambiguous.clone()),
        );
        let accepted = vec![Id::new("acknowledged-prefix")];

        let failure = SendError::grouped(accepted.clone(), source);

        assert_eq!(failure.accepted, accepted);
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, Some(ambiguous));
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(failure.to_string(), "grouped submission outcome is unknown");
    }
}
