use std::{error, fmt, future::Future, pin::Pin, sync::Arc};

use base::{Decimal, Id};

use crate::{AddressText, Error, Wallet};

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Id>, SendError>> + Send + 'a>>;

/// One requested value transfer from an already-created wallet.
pub struct Transfer {
    pub wallet: Arc<dyn Wallet>,
    pub to: AddressText,
    pub amount: Decimal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendError {
    pub accepted: Vec<Id>,
    pub failed_index: usize,
    pub source: Error,
}

impl SendError {
    #[must_use]
    pub fn at(failed_index: usize, accepted: Vec<Id>, source: Error) -> Self {
        Self {
            accepted,
            failed_index,
            source,
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {} failed after {} accepted transaction(s): {}",
            self.failed_index,
            self.accepted.len(),
            self.source
        )
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
