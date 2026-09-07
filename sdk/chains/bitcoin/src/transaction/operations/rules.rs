use std::collections::BTreeSet;

use bitcoin::ScriptBuf;

use crate::{ChainError, Network, Output, SpendSource};

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
