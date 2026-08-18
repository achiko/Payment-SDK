use crate::{Address, Wei};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction {
    pub chain_id: u64,
    pub nonce: u64,
    pub from: Address,
    pub to: Option<Address>,
    pub value: Wei,
    pub input: Vec<u8>,
    pub gas_limit: u64,
    pub max_fee_per_gas: Wei,
    pub max_priority_fee_per_gas: Wei,
}
