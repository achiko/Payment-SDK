use bitcoin::Network as NativeNetwork;

use crate::{ChainError, ChainErrorKind, Network};

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

pub(crate) const fn native_network(network: Network) -> NativeNetwork {
    match network {
        Network::Mainnet => NativeNetwork::Bitcoin,
        Network::Testnet3 => NativeNetwork::Testnet,
        Network::Testnet4 => NativeNetwork::Testnet4,
        Network::Signet => NativeNetwork::Signet,
        Network::Regtest => NativeNetwork::Regtest,
    }
}

pub(super) fn invalid_transaction(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

fn signer_error(error: base::SignerError) -> ChainError {
    signer_error_message(format!("Bitcoin signing failed: {error}"))
}

fn signer_error_message(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::Signer,
        message: message.into(),
    }
}
