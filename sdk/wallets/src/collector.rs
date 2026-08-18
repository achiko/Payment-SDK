use std::sync::Arc;

use base::{Address, Decimal, SignedTransaction};
use indexing::OutputId;

use crate::{Error, FutureResult, Wallet};

/// One exact canonical output approved for a UTXO collection transaction.
///
/// The concrete chain reloads the output from indexing and verifies this
/// amount before signing. Locking scripts and other chain evidence remain in
/// the concrete chain rather than crossing this boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedOutput {
    pub output: OutputId,
    pub amount: Decimal,
}

/// Result of preparing one collection transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedCollection {
    pub transaction: SignedTransaction,
    pub fee: PreparedFee,
}

/// Fee knowledge available before a transaction is broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparedFee {
    /// Exact fee encoded by a UTXO transaction's inputs and outputs.
    Exact(Decimal),
    /// Maximum account-chain fee authorized by the signed transaction.
    /// Receipt facts, rather than this ceiling, determine final accounting.
    Limit(Decimal),
}

/// Builds one exact multi-owner UTXO drain transaction.
pub trait Collector: Send {
    fn source(
        &mut self,
        wallet: Arc<dyn Wallet>,
        outputs: Vec<SelectedOutput>,
    ) -> Result<(), Error>;

    fn destination(&mut self, address: Address) -> Result<(), Error>;

    fn prepare<'a>(&'a mut self) -> FutureResult<'a, PreparedCollection>;
}

/// Prepares a full-balance drain for an account-model wallet.
///
/// The concrete wallet owns fee estimation and asset-specific rules. For a
/// native asset it subtracts the maximum network fee from the transferred
/// value. For a token it transfers the full token balance and verifies the
/// separately held native balance can pay the fee.
pub trait Sweeper: Send + Sync {
    fn sweep<'a>(&'a self, _destination: Address) -> FutureResult<'a, PreparedCollection> {
        Box::pin(async {
            Err(Error::new(
                crate::ErrorKind::Unsupported,
                "wallet does not support account-balance sweeping",
            ))
        })
    }
}
