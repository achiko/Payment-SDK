use std::collections::BTreeSet;

use base::Decimal;

use crate::{
    AssetId, BlockHeight, BlockRef, BoxFuture, CanonicalAddress, IndexError, IndexErrorKind,
    IndexScope, TransactionRef,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MovementId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MovementKind {
    Transfer,
    Input,
    Output,
    Mint,
    Burn,
}

/// One independently meaningful value change within a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueMovement {
    Transfer {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        from: CanonicalAddress,
        to: CanonicalAddress,
    },
    Input {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        owner: Option<CanonicalAddress>,
    },
    Output {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        owner: Option<CanonicalAddress>,
    },
    Mint {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        to: CanonicalAddress,
    },
    Burn {
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        from: CanonicalAddress,
    },
}

impl ValueMovement {
    #[must_use]
    pub fn id(&self) -> &MovementId {
        match self {
            Self::Transfer { id, .. }
            | Self::Input { id, .. }
            | Self::Output { id, .. }
            | Self::Mint { id, .. }
            | Self::Burn { id, .. } => id,
        }
    }

    #[must_use]
    pub fn asset(&self) -> &AssetId {
        match self {
            Self::Transfer { asset, .. }
            | Self::Input { asset, .. }
            | Self::Output { asset, .. }
            | Self::Mint { asset, .. }
            | Self::Burn { asset, .. } => asset,
        }
    }

    #[must_use]
    pub fn amount(&self) -> &Decimal {
        match self {
            Self::Transfer { amount, .. }
            | Self::Input { amount, .. }
            | Self::Output { amount, .. }
            | Self::Mint { amount, .. }
            | Self::Burn { amount, .. } => amount,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> MovementKind {
        match self {
            Self::Transfer { .. } => MovementKind::Transfer,
            Self::Input { .. } => MovementKind::Input,
            Self::Output { .. } => MovementKind::Output,
            Self::Mint { .. } => MovementKind::Mint,
            Self::Burn { .. } => MovementKind::Burn,
        }
    }

    #[must_use]
    pub fn from(&self) -> Option<&CanonicalAddress> {
        match self {
            Self::Transfer { from, .. } | Self::Burn { from, .. } => Some(from),
            Self::Input { owner, .. } => owner.as_ref(),
            Self::Output { .. } | Self::Mint { .. } => None,
        }
    }

    #[must_use]
    pub fn to(&self) -> Option<&CanonicalAddress> {
        match self {
            Self::Transfer { to, .. } | Self::Mint { to, .. } => Some(to),
            Self::Output { owner, .. } => owner.as_ref(),
            Self::Input { .. } | Self::Burn { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkFee {
    pub asset: AssetId,
    pub amount: Decimal,
    pub payer: Option<CanonicalAddress>,
}

/// Canonical state stored for a transaction. Confirmation is deliberately absent:
/// it is derived from the inclusion block and the current checkpoint when read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalStatus {
    Included {
        block: BlockRef,
    },
    Failed {
        block: BlockRef,
        reason: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalTransaction {
    pub scope: IndexScope,
    pub transaction_id: TransactionRef,
    pub status: CanonicalStatus,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
}

impl CanonicalTransaction {
    #[must_use]
    pub fn block(&self) -> &BlockRef {
        match &self.status {
            CanonicalStatus::Included { block } | CanonicalStatus::Failed { block, .. } => block,
        }
    }

    #[must_use]
    pub fn addresses(&self) -> BTreeSet<CanonicalAddress> {
        let mut addresses = self
            .movements
            .iter()
            .flat_map(|movement| {
                movement
                    .from()
                    .cloned()
                    .into_iter()
                    .chain(movement.to().cloned())
            })
            .collect::<BTreeSet<_>>();
        if let Some(payer) = self.fee.as_ref().and_then(|fee| fee.payer.clone()) {
            addresses.insert(payer);
        }
        addresses
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
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
pub struct ObservedTransaction {
    pub scope: IndexScope,
    pub transaction_id: TransactionRef,
    pub status: TransactionStatus,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationDraft {
    pub scope: IndexScope,
    pub transaction_id: TransactionRef,
    pub status: ObservationDraftStatus,
    pub movements: Vec<ValueMovement>,
    pub fee: Option<NetworkFee>,
}

impl ObservationDraft {
    pub(crate) fn canonical(
        self,
        scope: &IndexScope,
        block: &BlockRef,
    ) -> Result<CanonicalTransaction, IndexError> {
        if self.scope != *scope || !self.transaction_id.belongs_to(scope) {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "transaction belongs to another scope",
                false,
            ));
        }
        if matches!(self.status, ObservationDraftStatus::Failed { .. })
            && !self.movements.is_empty()
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "failed transaction contains movements",
                false,
            ));
        }
        let mut ids = BTreeSet::new();
        for movement in &self.movements {
            if movement.id().0.is_empty()
                || !ids.insert(movement.id())
                || movement.asset().chain != scope.chain
                || movement.amount().validate_amount().is_err()
                || movement
                    .from()
                    .is_some_and(|value| !value.belongs_to(scope))
                || movement.to().is_some_and(|value| !value.belongs_to(scope))
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "transaction contains an invalid movement",
                    false,
                ));
            }
        }
        if self.fee.as_ref().is_some_and(|fee| {
            fee.asset.chain != scope.chain
                || fee.amount.validate_amount().is_err()
                || fee
                    .payer
                    .as_ref()
                    .is_some_and(|payer| !payer.belongs_to(scope))
        }) {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "transaction contains an invalid network fee",
                false,
            ));
        }
        let status = match self.status {
            ObservationDraftStatus::Included => CanonicalStatus::Included {
                block: block.clone(),
            },
            ObservationDraftStatus::Failed { reason } => CanonicalStatus::Failed {
                block: block.clone(),
                reason,
            },
        };
        Ok(CanonicalTransaction {
            scope: self.scope,
            transaction_id: self.transaction_id,
            status,
            movements: self.movements,
            fee: self.fee,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationDraftStatus {
    Included,
    Failed { reason: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPosition {
    pub height: BlockHeight,
    pub transaction: TransactionRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryCursor {
    pub checkpoint: Option<BlockRef>,
    pub position: HistoryPosition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryQuery {
    pub scope: IndexScope,
    pub address: CanonicalAddress,
    pub after: Option<HistoryCursor>,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalPage {
    pub checkpoint: Option<BlockRef>,
    pub transactions: Vec<CanonicalTransaction>,
    pub next: Option<HistoryCursor>,
}

/// Address-primary canonical transaction history.
pub trait Transactions: Send + Sync {
    /// Lists one checkpoint-consistent page for an address.
    fn list<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionPage {
    pub checkpoint: Option<BlockRef>,
    pub transactions: Vec<ObservedTransaction>,
    pub next: Option<HistoryCursor>,
}
