use crate::{CollectionId, DepositError, DepositId};
use chain_identity::{AssetId, AtomicAmount};
use indexing::{MovementId, ObservationEvent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedMovement {
    pub movement_id: MovementId,
    pub asset: AssetId,
    pub amount: AtomicAmount,
}

/// PS semantics derived by consulting deposits and collection records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationClassification {
    Incoming {
        deposit_id: DepositId,
        credits: Vec<ClassifiedMovement>,
    },
    Collection {
        collection_id: CollectionId,
        deposit_id: DepositId,
        debits: Vec<ClassifiedMovement>,
        master_credits: Vec<ClassifiedMovement>,
        allocated_fee: Option<AtomicAmount>,
    },
    GasFunding {
        collection_id: CollectionId,
        deposit_id: DepositId,
        credits: Vec<ClassifiedMovement>,
    },
    RelevantButUnclassified,
}

pub trait ObservationClassifier: Send + Sync {
    fn classify(
        &self,
        event: &ObservationEvent,
    ) -> Result<Vec<ObservationClassification>, DepositError>;
}
