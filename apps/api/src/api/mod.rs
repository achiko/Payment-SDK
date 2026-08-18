mod contract;
mod error;
mod health;
mod transaction;
mod wallet;

use std::sync::Arc;

use crate::Gateway;
use axum::{Extension, Router};
use utoipa_axum::router::OpenApiRouter;

pub fn router(
    state: Gateway,
    config: &http_support::server::Config,
) -> Result<Router, http_support::server::ConfigError> {
    let protected = OpenApiRouter::new()
        .merge(wallet::routes())
        .merge(transaction::routes());
    let public = OpenApiRouter::new()
        .merge(health::routes())
        .merge(contract::routes());

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
