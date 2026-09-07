use std::collections::BTreeSet;

use bitcoin::{ScriptBuf, TapSighashType};

use crate::{ChainError, Network, Output, SighashType, SpendSource};

pub(super) fn checked_output(
    network: Network,
    output: &Output,
    allow_zero: bool,
) -> Result<ScriptBuf, ChainError> {
    let script = output.address.script_pubkey_for_network(network)?;
    let minimum = script.minimal_non_dust().to_sat();
    if !allow_zero && output.value.0 < minimum {
        return Err(ChainError::invalid_transaction(format!(
            "Bitcoin recipient output is dust: minimum is {minimum} satoshis"
        )));
    }
    Ok(script)
}

pub(super) fn validate_unique_utxos(utxos: &[SpendSource]) -> Result<(), ChainError> {
    let mut seen = BTreeSet::new();
    for utxo in utxos {
        if !seen.insert((utxo.transaction_id, utxo.output_index)) {
            return Err(ChainError::invalid_transaction(
                "Bitcoin transfer contains a duplicate UTXO",
            ));
        }
    }
    Ok(())
}

pub(super) fn sum_utxos(utxos: &[SpendSource]) -> Result<u64, ChainError> {
    utxos.iter().try_fold(0_u64, |total, utxo| {
        total.checked_add(utxo.value.0).ok_or_else(|| {
            ChainError::invalid_transaction("Bitcoin selected input amount overflowed u64")
        })
    })
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
