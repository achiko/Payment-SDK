use indexing::BlockRef;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinTransactionId(pub [u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinSignedTransaction {
    pub id: BitcoinTransactionId,
    pub consensus_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinReceipt {
    pub id: BitcoinTransactionId,
    pub included_in: Option<BlockRef>,
    pub confirmations: u64,
    pub replaced_by: Option<BitcoinTransactionId>,
}
