use std::sync::Arc;

use wallets::SendFuture;

use crate::{Address, Lamport};

use super::Key;

pub struct NativeTransfer {
    source: Address,
    signer: Arc<Key>,
    destination: Address,
    amount: Lamport,
}

impl NativeTransfer {
    pub(super) fn new(
        source: Address,
        signer: Arc<Key>,
        destination: Address,
        amount: Lamport,
    ) -> Result<Self, wallets::Error> {
        if signer.address() != &source {
            return Err(wallets::Error::new(
                wallets::ErrorKind::AddressMismatch,
                "Solana signer does not own the source address",
            ));
        }
        Ok(Self {
            source,
            signer,
            destination,
            amount,
        })
    }

    pub(crate) const fn source(&self) -> &Address {
        &self.source
    }

    pub(crate) fn signer(&self) -> Arc<Key> {
        Arc::clone(&self.signer)
    }

    pub(crate) const fn destination(&self) -> &Address {
        &self.destination
    }

    pub(crate) const fn amount(&self) -> Lamport {
        self.amount
    }
}

/// One native SOL submission path shared by single and batch adapters.
///
/// Implementations must enter the same source-keyed coordinator used by the
/// registered-family batch sender. This boundary deliberately cannot expose a
/// prepared envelope or create an independent source guard.
pub trait NativeSender: Send + Sync {
    fn send<'a>(&'a self, transfers: Vec<NativeTransfer>) -> SendFuture<'a>;
}
