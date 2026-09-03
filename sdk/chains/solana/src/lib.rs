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
pub use identity::{AssetKind, WalletConfig};
pub use indexer::{Block, BlockInterpreter, Source, SourceBudget};
pub use lamport::{Lamport, LamportError, LamportErrorKind};
pub use rpc::{
    Client as RpcClient, Commitment as RpcCommitment, Config as RpcConfig, Context as RpcContext,
    GenesisHash, SignatureStatus,
};
pub use transaction::{
    AcquiredAccounts, Acquirer, BlockhashLifetime, Cancellation, Coordinator, Envelope, Memo,
    Message, NativeDestination, PreparedBatch, Preparer, Reconciler, RegistrationError,
    ResolvedTransfer, SourceCoordinator, SubmissionRegistrar, SubmissionTask, Submitter,
};
pub use wallet::{
    AccountSnapshot, Key, NativeSender, NativeTransfer, Seed, SignedMessage, WalletProvider,
};

use base::{Asset, Chain, NetworkId, NetworkKind};

/// Canonical chain key shared by metadata, indexing scopes, and persistence.
pub const CHAIN: &str = "solana";

// Static native-SOL metadata does not replace the runtime network slug and
// expected genesis hash that bind each configured indexing scope.
const MAINNET: Chain = Chain::new(
    NetworkId::new("mainnet-beta", NetworkKind::Mainnet),
    CHAIN,
    "SOL",
);

/// Canonical native SOL metadata and decimal precision.
pub const SOL: Asset = Asset::new(MAINNET, "Solana", "SOL", 9);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_owns_canonical_native_sol_metadata() {
        assert_eq!(CHAIN, "solana");
        assert_eq!(SOL.chain.name, CHAIN);
        assert_eq!(SOL.chain.network_id.value(), &"mainnet-beta");
        assert!(SOL.chain.network_id.is_mainnet());
        assert_eq!(SOL.name, "Solana");
        assert_eq!(SOL.ticker, "SOL");
        assert_eq!(SOL.decimals, 9);
    }
}
