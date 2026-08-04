use crate::BoxFuture;
use chain_contract::ChainError;
use signer::Signer;

use super::{BitcoinInput, BitcoinOutput, BitcoinSignedTransaction, SighashType};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedBitcoinTransaction {
    pub version: i32,
    pub lock_time: u32,
    pub inputs: Vec<BitcoinInput>,
    pub outputs: Vec<BitcoinOutput>,
    pub sighash_type: SighashType,
}

/// Bitcoin owns sighash computation, signer invocation, and witness/script assembly.
pub trait BitcoinTransactionSigning: Send + Sync {
    fn sign<'a>(
        &'a self,
        transaction: UnsignedBitcoinTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<BitcoinSignedTransaction, ChainError>>;
}
