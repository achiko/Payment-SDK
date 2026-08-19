use std::{future::Future, pin::Pin};

use base::{Addresser, Broadcaster, Decimal, Signer, TransactionBuilder, TransactionId};
use indexing::{
    AssetId, BlockRef, CanonicalAddress, HistoryCursor, IndexScope, MovementId, MovementKind,
    ObservedTransaction, TransactionRef, TransactionStatus, ValueMovement,
};

use crate::{AddressFormat, Error};

pub type FutureResult<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    pub amount: Decimal,
    pub observed_at: Option<BlockRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRequest {
    pub after: Option<HistoryCursor>,
    pub limit: usize,
}

impl HistoryRequest {
    #[must_use]
    pub const fn first(limit: usize) -> Self {
        Self { after: None, limit }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    pub checkpoint: Option<BlockRef>,
    pub transactions: Vec<HistoryEntry>,
    pub next: Option<HistoryCursor>,
}

/// Asset metadata attached to every wallet-facing amount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryAsset {
    pub id: AssetId,
    pub name: Option<String>,
    pub ticker: Option<String>,
    pub decimals: u32,
}

impl HistoryAsset {
    pub fn display_amount(&self, atomic: &Decimal) -> Result<Decimal, Error> {
        let units = atomic.to_atomic(0).map_err(|error| {
            Error::new(
                crate::ErrorKind::History,
                format!("indexed amount is not a non-negative integer: {error}"),
            )
        })?;
        Ok(Decimal::from_atomic(units, self.decimals))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryMovement {
    pub id: MovementId,
    pub kind: MovementKind,
    pub asset: HistoryAsset,
    pub amount: Decimal,
    pub from: Option<CanonicalAddress>,
    pub to: Option<CanonicalAddress>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryFee {
    pub asset: HistoryAsset,
    pub amount: Decimal,
    pub payer: Option<CanonicalAddress>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryStatus {
    Included {
        block: BlockRef,
        confirmations: u64,
    },
    Confirmed {
        block: BlockRef,
        confirmations: u64,
    },
    Failed {
        block: BlockRef,
        reason: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub scope: IndexScope,
    pub transaction_id: TransactionRef,
    pub status: HistoryStatus,
    pub movements: Vec<HistoryMovement>,
    pub fee: Option<HistoryFee>,
}

impl History {
    /// Converts exact atomic indexing facts into wallet display units. The
    /// concrete wallet resolves asset precision and trusted display metadata.
    pub fn from_index<F>(
        page: indexing::TransactionPage,
        expected_scope: &IndexScope,
        asset: F,
    ) -> Result<Self, Error>
    where
        F: Fn(&AssetId) -> Result<HistoryAsset, Error>,
    {
        if page
            .next
            .as_ref()
            .is_some_and(|next| !next.position.transaction.belongs_to(expected_scope))
        {
            return Err(history_error(
                "indexed history cursor does not belong to the requested scope",
            ));
        }
        let transactions = page
            .transactions
            .into_iter()
            .map(|transaction| HistoryEntry::from_index(transaction, expected_scope, &asset))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            checkpoint: page.checkpoint,
            transactions,
            next: page.next,
        })
    }
}

impl HistoryEntry {
    fn from_index<F>(
        transaction: ObservedTransaction,
        expected_scope: &IndexScope,
        asset: &F,
    ) -> Result<Self, Error>
    where
        F: Fn(&AssetId) -> Result<HistoryAsset, Error>,
    {
        if &transaction.scope != expected_scope {
            return Err(history_error(
                "indexed transaction does not belong to the requested scope",
            ));
        }
        if !transaction.transaction_id.belongs_to(expected_scope) {
            return Err(history_error(
                "indexed transaction identity does not belong to its observation scope",
            ));
        }
        let scope = transaction.scope;
        let movements = transaction
            .movements
            .into_iter()
            .map(|movement| map_movement(movement, &scope, asset))
            .collect::<Result<Vec<_>, _>>()?;
        let fee = transaction
            .fee
            .map(|fee| {
                validate_asset_scope(&fee.asset, &scope)?;
                if fee
                    .payer
                    .as_ref()
                    .is_some_and(|payer| !payer.belongs_to(&scope))
                {
                    return Err(history_error(
                        "indexed fee payer does not belong to the transaction scope",
                    ));
                }
                let metadata = resolve_asset(&fee.asset, asset)?;
                Ok(HistoryFee {
                    amount: metadata.display_amount(&fee.amount)?,
                    asset: metadata,
                    payer: fee.payer,
                })
            })
            .transpose()?;
        Ok(Self {
            scope,
            transaction_id: transaction.transaction_id,
            status: transaction.status.into(),
            movements,
            fee,
        })
    }
}

impl From<TransactionStatus> for HistoryStatus {
    fn from(status: TransactionStatus) -> Self {
        match status {
            TransactionStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block,
                confirmations,
            },
            TransactionStatus::Confirmed {
                block,
                confirmations,
            } => Self::Confirmed {
                block,
                confirmations,
            },
            TransactionStatus::Failed { block, reason } => Self::Failed { block, reason },
        }
    }
}

fn map_movement<F>(
    movement: ValueMovement,
    scope: &IndexScope,
    asset: &F,
) -> Result<HistoryMovement, Error>
where
    F: Fn(&AssetId) -> Result<HistoryAsset, Error>,
{
    let kind = movement.kind();
    let (id, asset_id, atomic, from, to) = match movement {
        ValueMovement::Transfer {
            id,
            asset,
            amount,
            from,
            to,
        } => (id, asset, amount, Some(from), Some(to)),
        ValueMovement::Input {
            id,
            asset,
            amount,
            owner,
        } => (id, asset, amount, owner, None),
        ValueMovement::Output {
            id,
            asset,
            amount,
            owner,
        } => (id, asset, amount, None, owner),
        ValueMovement::Mint {
            id,
            asset,
            amount,
            to,
        } => (id, asset, amount, None, Some(to)),
        ValueMovement::Burn {
            id,
            asset,
            amount,
            from,
        } => (id, asset, amount, Some(from), None),
    };
    validate_asset_scope(&asset_id, scope)?;
    if from
        .iter()
        .chain(to.iter())
        .any(|address| !address.belongs_to(scope))
    {
        return Err(history_error(
            "indexed movement address does not belong to the transaction scope",
        ));
    }
    let metadata = resolve_asset(&asset_id, asset)?;
    Ok(HistoryMovement {
        id,
        kind,
        amount: metadata.display_amount(&atomic)?,
        asset: metadata,
        from,
        to,
    })
}

fn resolve_asset<F>(asset_id: &AssetId, asset: &F) -> Result<HistoryAsset, Error>
where
    F: Fn(&AssetId) -> Result<HistoryAsset, Error>,
{
    let metadata = asset(asset_id)?;
    if metadata.id != *asset_id {
        return Err(history_error(
            "wallet asset metadata does not match the indexed asset identity",
        ));
    }
    Ok(metadata)
}

fn validate_asset_scope(asset: &AssetId, scope: &IndexScope) -> Result<(), Error> {
    if asset.chain != scope.chain {
        return Err(history_error(
            "indexed asset does not belong to the transaction chain",
        ));
    }
    Ok(())
}

fn history_error(message: impl Into<String>) -> Error {
    Error::new(crate::ErrorKind::History, message)
}

pub trait BalanceReader: Send + Sync {
    fn balance<'a>(&'a self) -> FutureResult<'a, Balance>;
}

pub trait TransactionFactory: Send + Sync {
    fn transaction(&self) -> Box<dyn TransactionBuilder>;

    /// Restores durable intent while reinjecting this wallet's signer and RPC.
    fn restore(
        &self,
        snapshot: &base::TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, base::TransactionError>;

    fn broadcaster(&self) -> &dyn Broadcaster;
}

pub trait HistoryReader: Send + Sync {
    fn history<'a>(&'a self, request: HistoryRequest) -> FutureResult<'a, History>;
}

/// Chain-independent capabilities available after application composition.
pub trait Wallet:
    Addresser
    + AddressFormat
    + BalanceReader
    + TransactionFactory
    + HistoryReader
    + Signer
    + Send
    + Sync
{
    /// Builds, signs, and submits one transfer through this wallet's native
    /// transaction implementation. Inclusion and confirmation remain indexing
    /// facts rather than RPC results.
    fn send<'a>(
        &'a self,
        destination: crate::AddressText,
        amount: Decimal,
    ) -> FutureResult<'a, TransactionId> {
        Box::pin(async move {
            if amount <= Decimal::zero() {
                return Err(Error::new(
                    crate::ErrorKind::InvalidAmount,
                    "amount must be positive",
                ));
            }
            let destination = self.parse_address(&destination)?;
            let mut transaction = self.transaction();
            transaction.transfer(destination, amount)?;
            let signed = transaction.prepare().await?;
            let submitted = self.broadcaster().broadcast(&signed).await?;
            if submitted.id != *signed.id() {
                return Err(Error::new(
                    crate::ErrorKind::Transaction,
                    "broadcaster returned a different transaction ID",
                ));
            }
            Ok(submitted.id)
        })
    }
}

impl<T> Wallet for T where
    T: Addresser
        + AddressFormat
        + BalanceReader
        + TransactionFactory
        + HistoryReader
        + Signer
        + Send
        + Sync
{
}

#[cfg(test)]
mod tests {
    use indexing::{
        BlockHash, BlockHeight, CanonicalAddress, ChainId, HistoryCursor, HistoryPosition,
        IndexScope, MovementId, NetworkFee, TransactionPage, TransactionRef,
    };

    use super::*;

    fn asset(id: &str, decimals: u32) -> HistoryAsset {
        HistoryAsset {
            id: AssetId {
                chain: ChainId("example".to_owned()),
                asset: id.to_owned(),
            },
            name: None,
            ticker: None,
            decimals,
        }
    }

    fn address(value: &str) -> CanonicalAddress {
        CanonicalAddress {
            scope: scope(),
            value: value.to_owned(),
        }
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("example".to_owned()),
            network: "mainnet".to_owned(),
        }
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8; 32]),
            parent_hash: None,
            timestamp: None,
        }
    }

    #[test]
    fn converts_each_atomic_asset_with_its_own_precision() {
        let transaction = ObservedTransaction {
            scope: scope(),
            transaction_id: TransactionRef {
                scope: scope(),
                value: "tx".to_owned(),
            },
            status: TransactionStatus::Included {
                block: block(7),
                confirmations: 1,
            },
            movements: vec![ValueMovement::Transfer {
                id: MovementId("transfer".to_owned()),
                asset: AssetId {
                    chain: ChainId("example".to_owned()),
                    asset: "token".to_owned(),
                },
                amount: Decimal::from(1_500_000_u64),
                from: address("from"),
                to: address("to"),
            }],
            fee: Some(NetworkFee {
                asset: AssetId {
                    chain: ChainId("example".to_owned()),
                    asset: "native".to_owned(),
                },
                amount: Decimal::from(21_000_000_000_000_u64),
                payer: Some(address("from")),
            }),
        };

        let history = History::from_index(
            TransactionPage {
                checkpoint: Some(block(7)),
                transactions: vec![transaction],
                next: Some(HistoryCursor {
                    checkpoint: Some(block(7)),
                    position: HistoryPosition {
                        height: BlockHeight(7),
                        transaction: TransactionRef {
                            scope: scope(),
                            value: "next".to_owned(),
                        },
                    },
                }),
            },
            &scope(),
            |value| match value.asset.as_str() {
                "token" => Ok(asset("token", 6)),
                "native" => Ok(asset("native", 18)),
                _ => unreachable!("fixture uses known assets"),
            },
        )
        .expect("exact atomic amounts must convert");

        assert_eq!(
            history.transactions[0].movements[0].amount.to_string(),
            "1.5"
        );
        assert_eq!(
            history.transactions[0]
                .fee
                .as_ref()
                .expect("fee")
                .amount
                .to_string(),
            "0.000021"
        );
        assert_eq!(
            history
                .next
                .as_ref()
                .map(|next| next.position.transaction.value.as_str()),
            Some("next")
        );
        assert_eq!(history.transactions[0].scope, scope());
        assert_eq!(history.transactions[0].transaction_id.scope, scope());
        assert_eq!(
            history.transactions[0].movements[0]
                .from
                .as_ref()
                .expect("sender")
                .scope,
            scope()
        );
    }

    #[test]
    fn rejects_non_integer_index_amounts() {
        let error = asset("native", 8)
            .display_amount(&"0.1".parse().expect("decimal"))
            .expect_err("index persistence must contain atomic integers");

        assert_eq!(error.kind, crate::ErrorKind::History);
    }
}
