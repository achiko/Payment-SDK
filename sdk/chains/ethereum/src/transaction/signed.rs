use indexing::BlockRef;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumTransactionId(pub [u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumSignedTransaction {
    pub id: EthereumTransactionId,
    pub envelope: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumReceipt {
    pub id: EthereumTransactionId,
    pub included_in: Option<BlockRef>,
    pub succeeded: Option<bool>,
    pub confirmations: u64,
}
