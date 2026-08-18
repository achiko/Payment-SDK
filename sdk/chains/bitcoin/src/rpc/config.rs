use indexing::{BlockHash, SourceError};

use crate::Network;

use super::source_error;

/// Strict Bitcoin Core identity and readiness requirements shared by wallets and indexers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreConfig {
    pub expected_network: Network,
    pub expected_genesis_hash: BlockHash,
}

impl CoreConfig {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.expected_genesis_hash.0.len() != 32 {
            return Err(source_error(
                "configured Bitcoin genesis hash must be 32 bytes",
                false,
            ));
        }
        Ok(())
    }
}
