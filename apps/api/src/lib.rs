//! Public HTTP contract for the composed payment API.
//!
//! Concrete chains, persistence, synchronization, and wallet providers are
//! wired explicitly by the binary composition root. This library owns only the
//! transport state, routes, and wire models.

mod api;
pub use api::{
    Address, AddressEncoding, AddressInput, Asset, Balance, Block, Chain, CreateWallet, Fee,
    HistoryQuery, Movement, MovementKind, Scope, SendFunds, State, Status, Submission, Transaction,
    TransactionPage, TransferRequest, TransferResponse, Wallet, WalletTransfer, router,
};
