//! Pure, chain-independent UTXO selection and funding contracts.

use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount(pub u128);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeRate {
    pub units_per_weight: u64,
}

pub trait Utxo: Clone {
    type Id: Clone + Eq;

    fn id(&self) -> Self::Id;

    fn value(&self) -> Amount;

    fn satisfaction_weight(&self) -> u64;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipient<D> {
    pub destination: D,
    pub amount: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildRequest<U, D> {
    pub available: Vec<U>,
    pub recipients: Vec<Recipient<D>>,
    pub change_destination: D,
    pub fee_rate: FeeRate,
    pub minimum_change: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundedTransaction<U, D> {
    pub selected: Vec<U>,
    pub recipients: Vec<Recipient<D>>,
    pub change: Option<Recipient<D>>,
    pub fee: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection<U> {
    pub selected: Vec<U>,
    pub total: Amount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtxoBuildError {
    pub message: String,
}

impl fmt::Display for UtxoBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for UtxoBuildError {}

pub trait InputSelector<U: Utxo>: Send + Sync {
    fn select(
        &self,
        candidates: &[U],
        target: Amount,
        fee_rate: FeeRate,
    ) -> Result<Selection<U>, UtxoBuildError>;
}

pub trait TransactionBuilder<U: Utxo, D>: Send + Sync {
    fn build(&self, request: BuildRequest<U, D>)
    -> Result<FundedTransaction<U, D>, UtxoBuildError>;
}
