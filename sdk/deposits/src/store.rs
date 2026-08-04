use crate::{
    BoxFuture, Collection, CollectionId, CollectionLegId, CollectionLegState, CreateDeposit,
    Deposit, DepositError, DepositId, DepositState,
};
use chain_identity::CanonicalAddress;

/// Backend-independent PS persistence contract. No database engine is selected here.
pub trait DepositStore: Send + Sync {
    fn create<'a>(&'a self, command: CreateDeposit)
    -> BoxFuture<'a, Result<Deposit, DepositError>>;

    fn deposit<'a>(
        &'a self,
        id: &'a DepositId,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>>;

    fn by_address<'a>(
        &'a self,
        address: &'a CanonicalAddress,
    ) -> BoxFuture<'a, Result<Option<Deposit>, DepositError>>;

    fn set_state<'a>(
        &'a self,
        id: &'a DepositId,
        state: DepositState,
    ) -> BoxFuture<'a, Result<(), DepositError>>;
}

pub trait CollectionStore: Send + Sync {
    fn create_collection<'a>(
        &'a self,
        collection: Collection,
    ) -> BoxFuture<'a, Result<(), DepositError>>;

    fn collection<'a>(
        &'a self,
        id: &'a CollectionId,
    ) -> BoxFuture<'a, Result<Option<Collection>, DepositError>>;

    fn set_leg_state<'a>(
        &'a self,
        collection_id: &'a CollectionId,
        leg_id: &'a CollectionLegId,
        state: CollectionLegState,
    ) -> BoxFuture<'a, Result<(), DepositError>>;
}
