//! Solana-owned contracts and chain-native types.

mod address;
mod batch;
mod error;
mod identity;
mod indexer;
mod lamport;
mod rpc;
mod transaction;
mod wallet;

pub use address::{Address, AddressParseError};
pub use batch::Batch;
pub use error::{Error, ErrorKind};
pub use identity::NativeAsset;
pub use indexer::{BlockInterpreter, SourceBudget};
pub use lamport::{Lamport, LamportError, LamportErrorKind};
pub use rpc::{
    Client as RpcClient, Commitment as RpcCommitment, Config as RpcConfig, Context as RpcContext,
    GenesisHash,
};
pub use transaction::{
    AcquiredAccounts, Acquirer, BlockhashLifetime, Cancellation, Memo, NativeDestination,
    ResolvedTransfer, SourceCoordinator,
};
pub use wallet::{AccountSnapshot, Key, Seed, SignedMessage};

/// Canonical chain key shared by metadata, indexing scopes, and persistence.
pub const CHAIN: &str = "solana";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_owns_the_canonical_chain_key() {
        assert_eq!(CHAIN, "solana");
    }
}
