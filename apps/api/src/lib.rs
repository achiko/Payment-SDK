//! Authenticated wallet API facade.
//!
//! The process composes chain RPC, durable indexing, and wallet providers in
//! memory. HTTP handlers expose only generated wallet summaries, balances, and
//! history; protocol and persistence details remain behind SDK capabilities.

mod api;
mod dto;
mod error;
mod server;
pub use dto::{
    Address, AddressEncoding, AddressInput, Asset, Balance, Block, Chain, CreateWallet, Fee,
    HistoryQuery, Movement, MovementKind, Proof, Scope, ScopedId, SendFunds, Status, Submission,
    Transaction, TransactionPage, TransferRequest, TransferResponse, Wallet, WalletTransfer,
};
pub use error::{BatchError, Error, ErrorKind};
pub use server::{Gateway, WalletFamily, WalletSend};
