use std::sync::Arc;

use axum::Router;
use wallets::Wallet;

use crate::Server;

#[derive(Default)]
pub struct Service {
    server: Server,
    wallet_count: usize,
}

impl Service {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            server: Server::new(),
            wallet_count: 0,
        }
    }

    #[must_use]
    pub fn with(mut self, id: impl Into<String>, wallet: Arc<dyn Wallet>) -> Self {
        self.server = self.server.with(id, wallet);
        self.wallet_count += 1;
        self
    }

    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.wallet_count > 0
    }

    pub fn router(
        self,
        config: &http_support::server::Config,
    ) -> Result<Router, http_support::server::ConfigError> {
        self.server.router(config)
    }
}
