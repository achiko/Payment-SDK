use std::fmt::Debug;

pub trait Chain: Debug + Clone + Copy + Send + Sync + 'static {
    const NAME: &'static str;

    type Asset: Clone + Debug + Eq + Send + Sync + 'static;
    type Address: Clone + Debug + Eq + Send + Sync + 'static;
    type Amount: Clone + Debug + Send + Sync + 'static;
    type TransactionId: Clone + Debug + Eq + Send + Sync + 'static;
    type GenerateAddressRequest: Clone + Debug + Send + Sync + 'static;
    type TransferRequest: Clone + Debug + Send + Sync + 'static;
    type CollectionRequest: Clone + Debug + Send + Sync + 'static;
    type CollectionRequirement: Clone + Debug + Send + Sync + 'static;
    type CollectionAttribution: Clone + Debug + Send + Sync + 'static;
    type UnsignedTransaction: Clone + Debug + Send + Sync + 'static;
    type SignedTransaction: Clone + Debug + Send + Sync + 'static;
    type Receipt: Clone + Debug + Send + Sync + 'static;
}
