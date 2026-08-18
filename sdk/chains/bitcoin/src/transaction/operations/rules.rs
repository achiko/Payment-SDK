use std::collections::BTreeSet;

use bitcoin::{
    Address as NativeAddress, EcdsaSighashType, ScriptBuf, TapSighashType,
    address::NetworkUnchecked,
};

use crate::{Address, ChainError, ChainErrorKind, Network, Output, SighashType, SpendSource};

use super::{invalid_transaction, native_network};

pub(super) fn checked_output(
    network: Network,
    output: &Output,
    allow_zero: bool,
) -> Result<ScriptBuf, ChainError> {
    let address = checked_address(network, &output.address)?;
    let minimum = address.script_pubkey().minimal_non_dust().to_sat();
    if !allow_zero && output.value.0 < minimum {
        return Err(invalid_transaction(format!(
            "Bitcoin recipient output is dust: minimum is {minimum} satoshis"
        )));
    }
    Ok(address.script_pubkey())
}

pub(super) fn checked_address(
    network: Network,
    address: &Address,
) -> Result<NativeAddress, ChainError> {
    address
        .encoded()
        .parse::<NativeAddress<NetworkUnchecked>>()
        .map_err(|error| ChainError {
            kind: ChainErrorKind::InvalidAddress,
            message: format!("invalid Bitcoin address: {error}"),
        })?
        .require_network(native_network(network))
        .map_err(|error| ChainError {
            kind: ChainErrorKind::InvalidAddress,
            message: format!("Bitcoin address is for the wrong network: {error}"),
        })
}

pub(super) fn validate_unique_utxos(utxos: &[SpendSource]) -> Result<(), ChainError> {
    let mut seen = BTreeSet::new();
    for utxo in utxos {
        if !seen.insert((utxo.transaction_id, utxo.output_index)) {
            return Err(invalid_transaction(
                "Bitcoin transfer contains a duplicate UTXO",
            ));
        }
    }
    Ok(())
}

pub(super) fn sum_utxos(utxos: &[SpendSource]) -> Result<u64, ChainError> {
    utxos.iter().try_fold(0_u64, |total, utxo| {
        total
            .checked_add(utxo.value.0)
            .ok_or_else(|| invalid_transaction("Bitcoin selected input amount overflowed u64"))
    })
}

pub(super) fn ecdsa_sighash_type(
    sighash_type: SighashType,
) -> Result<EcdsaSighashType, ChainError> {
    match sighash_type {
        SighashType::All => Ok(EcdsaSighashType::All),
        SighashType::None => Ok(EcdsaSighashType::None),
        SighashType::Single => Ok(EcdsaSighashType::Single),
        SighashType::AllAnyoneCanPay => Ok(EcdsaSighashType::AllPlusAnyoneCanPay),
        SighashType::NoneAnyoneCanPay => Ok(EcdsaSighashType::NonePlusAnyoneCanPay),
        SighashType::SingleAnyoneCanPay => Ok(EcdsaSighashType::SinglePlusAnyoneCanPay),
        SighashType::TaprootDefault => Err(invalid_transaction(
            "Taproot default sighash cannot sign a P2WPKH input",
        )),
    }
}

pub(super) fn taproot_sighash_type(
    sighash_type: SighashType,
) -> Result<TapSighashType, ChainError> {
    match sighash_type {
        SighashType::All => Ok(TapSighashType::All),
        SighashType::None => Ok(TapSighashType::None),
        SighashType::Single => Ok(TapSighashType::Single),
        SighashType::AllAnyoneCanPay => Ok(TapSighashType::AllPlusAnyoneCanPay),
        SighashType::NoneAnyoneCanPay => Ok(TapSighashType::NonePlusAnyoneCanPay),
        SighashType::SingleAnyoneCanPay => Ok(TapSighashType::SinglePlusAnyoneCanPay),
        SighashType::TaprootDefault => Ok(TapSighashType::Default),
    }
}
