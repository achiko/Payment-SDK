//! Narrow account-model transaction construction contracts.
//!
//! This is not an assertion that Ethereum, Solana, and every other
//! account-oriented chain share one concrete transaction format.

use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer<A, V, P> {
    pub sender: A,
    pub recipient: Option<A>,
    pub value: V,
    pub payload: P,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildContext<N, F> {
    pub nonce: N,
    pub fee: F,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountBuildError {
    pub message: String,
}

impl fmt::Display for AccountBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for AccountBuildError {}

pub trait TransactionBuilder: Send + Sync {
    type Address;
    type Value;
    type Payload;
    type Nonce;
    type Fee;
    type UnsignedTransaction;

    fn build(
        &self,
        transfer: Transfer<Self::Address, Self::Value, Self::Payload>,
        context: BuildContext<Self::Nonce, Self::Fee>,
    ) -> Result<Self::UnsignedTransaction, AccountBuildError>;
}
