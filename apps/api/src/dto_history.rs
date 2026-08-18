use serde::{Deserialize, Serialize};

use super::HistoryCursor;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct TransactionPage {
    pub transactions: Vec<Transaction>,
    pub next_cursor: Option<String>,
}

impl TryFrom<wallets::History> for TransactionPage {
    type Error = crate::Error;

    fn try_from(history: wallets::History) -> Result<Self, Self::Error> {
        let transactions = history.transactions.into_iter().map(Into::into).collect();
        let next_cursor = history
            .next
            .as_ref()
            .map(HistoryCursor::encode)
            .transpose()?;
        Ok(Self {
            transactions,
            next_cursor,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Transaction {
    pub scope: Scope,
    pub transaction_id: ScopedId,
    pub revision: u64,
    pub status: Status,
    pub movements: Vec<Movement>,
    pub fee: Option<Fee>,
    pub first_seen_at: u64,
    pub observed_at: u64,
}

impl From<wallets::HistoryEntry> for Transaction {
    fn from(value: wallets::HistoryEntry) -> Self {
        Self {
            scope: value.scope.into(),
            transaction_id: value.transaction_id.into(),
            revision: value.revision.0,
            status: value.status.into(),
            movements: value.movements.into_iter().map(Into::into).collect(),
            fee: value.fee.map(Into::into),
            first_seen_at: value.first_seen_at,
            observed_at: value.observed_at,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Scope {
    pub chain: String,
    pub network: String,
}

impl From<indexing::IndexScope> for Scope {
    fn from(value: indexing::IndexScope) -> Self {
        Self {
            chain: value.chain.0,
            network: value.network,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct ScopedId {
    pub scope: Scope,
    pub value: String,
}

impl From<indexing::TransactionRef> for ScopedId {
    fn from(value: indexing::TransactionRef) -> Self {
        Self {
            scope: value.scope.into(),
            value: value.value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Address {
    pub scope: Scope,
    pub value: String,
}

impl From<indexing::CanonicalAddress> for Address {
    fn from(value: indexing::CanonicalAddress) -> Self {
        Self {
            scope: value.scope.into(),
            value: value.value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Asset {
    pub chain: String,
    pub id: String,
    pub name: Option<String>,
    pub ticker: Option<String>,
    pub decimals: u32,
}

impl From<wallets::HistoryAsset> for Asset {
    fn from(value: wallets::HistoryAsset) -> Self {
        Self {
            chain: value.id.chain.0,
            id: value.id.asset,
            name: value.name,
            ticker: value.ticker,
            decimals: value.decimals,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MovementKind {
    Transfer,
    Input,
    Output,
    InternalTransfer,
    Mint,
    Burn,
}

impl From<indexing::MovementKind> for MovementKind {
    fn from(value: indexing::MovementKind) -> Self {
        match value {
            indexing::MovementKind::Transfer => Self::Transfer,
            indexing::MovementKind::Input => Self::Input,
            indexing::MovementKind::Output => Self::Output,
            indexing::MovementKind::InternalTransfer => Self::InternalTransfer,
            indexing::MovementKind::Mint => Self::Mint,
            indexing::MovementKind::Burn => Self::Burn,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Movement {
    pub id: String,
    pub kind: MovementKind,
    pub asset: Asset,
    pub amount: String,
    pub from: Option<Address>,
    pub to: Option<Address>,
}

impl From<wallets::HistoryMovement> for Movement {
    fn from(value: wallets::HistoryMovement) -> Self {
        Self {
            id: value.id.0,
            kind: value.kind.into(),
            asset: value.asset.into(),
            amount: value.amount.to_string(),
            from: value.from.map(Into::into),
            to: value.to.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Fee {
    pub asset: Asset,
    pub amount: String,
    pub payer: Option<Address>,
}

impl From<wallets::HistoryFee> for Fee {
    fn from(value: wallets::HistoryFee) -> Self {
        Self {
            asset: value.asset.into(),
            amount: value.amount.to_string(),
            payer: value.payer.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Block {
    pub height: u64,
    pub hash: String,
    pub parent_hash: Option<String>,
    pub timestamp: Option<u64>,
}

impl From<base::BlockRef> for Block {
    fn from(value: base::BlockRef) -> Self {
        Self {
            height: value.height.0,
            hash: hex::encode(value.hash.0),
            parent_hash: value.parent_hash.map(|hash| hex::encode(hash.0)),
            timestamp: value.timestamp,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Proof {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

impl From<indexing::ConfirmationProof> for Proof {
    fn from(value: indexing::ConfirmationProof) -> Self {
        match value {
            indexing::ConfirmationProof::Depth { required, observed } => {
                Self::Depth { required, observed }
            }
            indexing::ConfirmationProof::ChainFinalized => Self::ChainFinalized,
            indexing::ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized { required, observed }
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    Pending,
    Included {
        block: Block,
        confirmations: u64,
    },
    Confirmed {
        block: Block,
        proof: Proof,
    },
    Failed {
        block: Option<Block>,
        reason: Option<String>,
    },
    Replaced {
        by: ScopedId,
    },
    Dropped,
    Reorged {
        previous_block: Block,
    },
}

impl From<wallets::HistoryStatus> for Status {
    fn from(value: wallets::HistoryStatus) -> Self {
        match value {
            wallets::HistoryStatus::Pending => Self::Pending,
            wallets::HistoryStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations,
            },
            wallets::HistoryStatus::Confirmed { block, proof } => Self::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            wallets::HistoryStatus::Failed { block, reason } => Self::Failed {
                block: block.map(Into::into),
                reason,
            },
            wallets::HistoryStatus::Replaced { by } => Self::Replaced { by: by.into() },
            wallets::HistoryStatus::Dropped => Self::Dropped,
            wallets::HistoryStatus::Reorged { previous_block } => Self::Reorged {
                previous_block: previous_block.into(),
            },
        }
    }
}
