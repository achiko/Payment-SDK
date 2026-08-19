//! Runtime wallet composition without concrete protocol dependencies.

mod address;
mod error;
mod provider;
mod sender;
mod wallet;
mod wallets;

pub use address::{AddressEncoding, AddressFormat, AddressText};
pub use crypto::SecretBytes;
pub use error::{Error, ErrorKind};
pub use provider::Provider;
pub use sender::{SendError, SendFuture, Sender, Transfer};
pub use wallet::{
    Balance, BalanceReader, FutureResult, History, HistoryAsset, HistoryEntry, HistoryFee,
    HistoryMovement, HistoryReader, HistoryRequest, HistoryStatus, TransactionFactory, Wallet,
};
pub use wallets::{WalletInfo, WalletTransfer, Wallets};
