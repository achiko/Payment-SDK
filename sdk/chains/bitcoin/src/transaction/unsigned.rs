use super::{Input, Output, SighashType};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction {
    pub version: i32,
    pub lock_time: u32,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub sighash_type: SighashType,
}
