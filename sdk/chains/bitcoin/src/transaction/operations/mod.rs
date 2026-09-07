use crate::{ChainError, ChainErrorKind};

use super::{
    BuildRequest, Funding, Input, Output, SighashType, SignedTransaction, SpendSource,
    TransactionId, UnsignedTransaction,
};

mod build;
mod rules;
mod sign;

pub(super) use build::build_grouped;
pub(super) use sign::{sign, sign_each};

use rules::{checked_output, sum_utxos, taproot_sighash_type, validate_unique_utxos};

fn signer_error(error: base::SignerError) -> ChainError {
    signer_error_message(format!("Bitcoin signing failed: {error}"))
}

fn signer_error_message(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::Signer,
        message: message.into(),
    }
}
