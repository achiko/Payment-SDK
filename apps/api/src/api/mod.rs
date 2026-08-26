mod contract;
mod cursor;
mod error;
mod health;
mod openapi;
mod transaction;
mod wallet;

pub use contract::{AddressEncoding, AddressInput, Chain, Wallet, WalletAsset};
pub use transaction::{
    Address, Asset, Block, Fee, HistoryQuery, Movement, MovementKind, Scope, SendFunds, Status,
    Submission, Transaction, TransactionPage, TransferRequest, TransferResponse, WalletTransfer,
};
pub use wallet::{Balance, CreateWallet};

use std::sync::Arc;

use axum::{Extension, Router};
use tokio::sync::watch;
use utoipa_axum::router::OpenApiRouter;

#[derive(Clone)]
pub struct State {
    wallets: Arc<wallets::Wallets<String, WalletAsset>>,
    readiness: watch::Receiver<bool>,
}

impl State {
    #[must_use]
    pub fn new(
        wallets: Arc<wallets::Wallets<String, WalletAsset>>,
        readiness: watch::Receiver<bool>,
    ) -> Self {
        Self { wallets, readiness }
    }
}

pub fn router(
    state: State,
    config: &http_support::server::Config,
) -> Result<Router, http_support::server::ConfigError> {
    let protected = OpenApiRouter::new()
        .merge(wallet::routes())
        .merge(transaction::routes());
    let public = OpenApiRouter::new()
        .merge(health::routes())
        .merge(openapi::routes());

    let (protected, mut contract) = protected.split_for_parts();
    let (public, public_contract) = public.split_for_parts();
    contract.merge(public_contract);
    let contract = Arc::new(contract);

    let protected =
        http_support::server::protected_router(protected.with_state(state.clone()), config)?;
    Ok(protected
        .merge(public.with_state(state))
        .layer(Extension(contract)))
}

#[cfg(test)]
#[path = "api_test.rs"]
mod tests;
