//! Small capabilities that each concrete chain exposes to applications.

mod chain;
mod error;
mod transaction;
mod wallet;

pub use chain::Chain;
pub use chain_identity::{
    AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId, SignedAtomicAmount,
};
pub use error::{ChainError, ChainErrorKind};
pub use transaction::{
    Broadcaster, CollectionResult, CollectionSubmission, Collector, TransactionReader,
    TransactionSigner, TransferBuilder,
};
pub use wallet::{
    Balance, BalanceReader, DepositAddressGenerator, GeneratedAddress, WalletAdapter, WalletFactory,
};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
