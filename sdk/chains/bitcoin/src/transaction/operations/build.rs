use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    hashes::Hash, transaction::Version,
};

use crate::{ChainError, ChainErrorKind, FeeRate, Network, Satoshi};

use super::{
    BuildRequest, Funding, Input, Output, SpendSource, UnsignedTransaction, checked_output,
    validate_unique_utxos,
};

const SEGWIT_MARKER_FLAG_WEIGHT: u64 = 2;

impl BuildRequest {
    pub(in crate::transaction) fn build(
        mut self,
        network: Network,
    ) -> Result<UnsignedTransaction, ChainError> {
        if self.available.is_empty() {
            return Err(ChainError::insufficient_funds(
                "Bitcoin transfer has no available UTXOs",
            ));
        }
        if self.recipients.is_empty() {
            return Err(ChainError::invalid_transaction(
                "Bitcoin transfer has no recipients",
            ));
        }
        if self.fee_rate.satoshis_per_kvb() == 0 {
            return Err(ChainError {
                kind: ChainErrorKind::FeeUnavailable,
                message: "Bitcoin fee rate must be greater than zero".to_owned(),
            });
        }
        validate_unique_utxos(&self.available)?;
        for utxo in &self.available {
            let script = ScriptBuf::from_bytes(utxo.script_pubkey.clone());
            if !script.is_p2wpkh() && !script.is_p2tr() {
                return Err(ChainError::invalid_transaction(
                    "Bitcoin wallet supports P2WPKH and P2TR inputs only",
                ));
            }
        }

        let recipient_scripts = self
            .recipients
            .iter()
            .map(|output| checked_output(network, output, self.drain_wallet))
            .collect::<Result<Vec<_>, ChainError>>()?;
        let change_script = self.change_address.script_pubkey_for_network(network)?;

        if self.drain_wallet {
            if self.recipients.len() != 1 {
                return Err(ChainError::invalid_transaction(
                    "Bitcoin drain transfer requires exactly one recipient",
                ));
            }
            self.available.sort_by(SpendSource::compare_outpoint);
            let selected_total = self
                .available
                .iter()
                .try_fold(0_u64, |total, utxo| total.checked_add(utxo.value.0))
                .ok_or_else(|| {
                    ChainError::invalid_transaction("Bitcoin selected input amount overflowed u64")
                })?;
            let fee = predicted_fee(&self.available, &recipient_scripts, self.fee_rate)?;
            let value = selected_total.checked_sub(fee).ok_or_else(|| {
                ChainError::insufficient_funds(
                    "Bitcoin UTXOs cannot cover the drain transaction fee",
                )
            })?;
            let minimum = recipient_scripts[0].minimal_non_dust().to_sat();
            if value < minimum {
                return Err(ChainError::insufficient_funds(format!(
                    "Bitcoin drain output is dust: minimum is {minimum} satoshis"
                )));
            }
            self.recipients[0].value = Satoshi(value);
            return Ok(unsigned(self.available, self.recipients));
        }

        self.available.sort_by(|left, right| {
            right
                .value
                .cmp(&left.value)
                .then_with(|| left.transaction_id.cmp(&right.transaction_id))
                .then_with(|| left.output_index.cmp(&right.output_index))
        });

        let recipient_total = self.recipients.iter().try_fold(0_u64, |total, output| {
            total.checked_add(output.value.0).ok_or_else(|| {
                ChainError::invalid_transaction("Bitcoin recipient amount overflowed u64")
            })
        })?;
        let mut selected = Vec::new();
        let mut selected_total = 0_u64;
        let mut final_outputs = None;

        for utxo in self.available {
            selected_total = selected_total.checked_add(utxo.value.0).ok_or_else(|| {
                ChainError::invalid_transaction("Bitcoin selected input amount overflowed u64")
            })?;
            selected.push(utxo);

            let fee_without_change = predicted_fee(&selected, &recipient_scripts, self.fee_rate)?;
            let required = recipient_total
                .checked_add(fee_without_change)
                .ok_or_else(|| {
                    ChainError::invalid_transaction("Bitcoin amount and fee overflowed u64")
                })?;
            if selected_total < required {
                continue;
            }

            let mut with_change_scripts = recipient_scripts.clone();
            with_change_scripts.push(change_script.clone());
            let fee_with_change = predicted_fee(&selected, &with_change_scripts, self.fee_rate)?;
            let change = selected_total
                .checked_sub(recipient_total)
                .and_then(|value| value.checked_sub(fee_with_change));
            let mut outputs = self.recipients.clone();
            if let Some(change) =
                change.filter(|value| *value >= change_script.minimal_non_dust().to_sat())
            {
                outputs.push(Output {
                    address: self.change_address.clone(),
                    value: Satoshi(change),
                });
            }
            final_outputs = Some(outputs);
            break;
        }

        let outputs = final_outputs.ok_or_else(|| {
            ChainError::insufficient_funds(format!(
                "insufficient Bitcoin funds for {recipient_total} satoshis plus network fee"
            ))
        })?;
        Ok(unsigned(selected, outputs))
    }
}

pub(in crate::transaction) fn build_grouped(
    network: Network,
    mut groups: Vec<Funding>,
    fee_rate: FeeRate,
) -> Result<UnsignedTransaction, ChainError> {
    if groups.is_empty() || fee_rate.satoshis_per_kvb() == 0 {
        return Err(ChainError::invalid_transaction(
            "Bitcoin grouped transfer needs sources and a positive fee rate",
        ));
    }
    let mut available = Vec::new();
    let mut recipients = Vec::new();
    let mut surplus = Vec::with_capacity(groups.len());
    let mut change = Vec::with_capacity(groups.len());
    for group in &mut groups {
        if group.available.is_empty() || group.recipients.is_empty() {
            return Err(ChainError::invalid_transaction(
                "each Bitcoin grouped source needs inputs and recipients",
            ));
        }
        group.available.sort_by(SpendSource::compare_outpoint);
        let input = group
            .available
            .iter()
            .try_fold(0_u64, |total, utxo| total.checked_add(utxo.value.0))
            .ok_or_else(|| {
                ChainError::invalid_transaction("Bitcoin selected input amount overflowed u64")
            })?;
        let output = group.recipients.iter().try_fold(0_u64, |sum, value| {
            sum.checked_add(value.value.0).ok_or_else(|| {
                ChainError::invalid_transaction("Bitcoin recipient amount overflowed u64")
            })
        })?;
        surplus.push(input.checked_sub(output).ok_or_else(|| {
            ChainError::insufficient_funds(
                "a Bitcoin grouped source cannot fund its requested outputs",
            )
        })?);
        change.push(group.change_address.script_pubkey_for_network(network)?);
        available.append(&mut group.available);
        recipients.append(&mut group.recipients);
    }
    validate_unique_utxos(&available)?;
    let recipient_scripts = recipients
        .iter()
        .map(|output| checked_output(network, output, false))
        .collect::<Result<Vec<_>, _>>()?;
    let mut active = surplus
        .iter()
        .zip(&change)
        .map(|(value, script)| *value >= script.minimal_non_dust().to_sat())
        .collect::<Vec<_>>();
    loop {
        let mut scripts = recipient_scripts.clone();
        scripts.extend(
            change
                .iter()
                .zip(&active)
                .filter_map(|(script, active)| active.then_some(script.clone())),
        );
        let fee = predicted_fee(&available, &scripts, fee_rate)?;
        let remaining = allocate_fee(&surplus, fee)?;
        let next = remaining
            .iter()
            .zip(change.iter().zip(&active))
            .map(|(value, (script, was_active))| {
                *was_active && *value >= script.minimal_non_dust().to_sat()
            })
            .collect::<Vec<_>>();
        if next == active {
            for ((group, value), keep) in groups.iter().zip(remaining).zip(next) {
                if keep {
                    recipients.push(Output::from_atomic(
                        group.change_address.clone(),
                        Satoshi(value),
                    ));
                }
            }
            return Ok(unsigned(available, recipients));
        }
        active = next;
    }
}

// design-lint: allow single-use-free-function -- isolates deterministic source-ordered fee allocation from grouped transaction assembly
// design-lint: allow unclassified-free-function -- deterministic allocation across source-ordered surpluses; no individual source or fee-rate value owns the aggregate policy
fn allocate_fee(surplus: &[u64], fee: u64) -> Result<Vec<u64>, ChainError> {
    let mut remaining_fee = fee;
    let remaining = surplus
        .iter()
        .map(|value| {
            let contribution = (*value).min(remaining_fee);
            remaining_fee -= contribution;
            value - contribution
        })
        .collect::<Vec<_>>();
    if remaining_fee != 0 {
        return Err(ChainError::insufficient_funds(
            "Bitcoin grouped sources cannot cover the network fee",
        ));
    }
    Ok(remaining)
}

fn unsigned(utxos: Vec<SpendSource>, outputs: Vec<Output>) -> UnsignedTransaction {
    UnsignedTransaction {
        version: 2,
        lock_time: 0,
        inputs: utxos
            .into_iter()
            .map(|utxo| Input {
                utxo,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32(),
            })
            .collect(),
        outputs,
        sighash_type: super::SighashType::All,
    }
}

fn predicted_fee(
    inputs: &[SpendSource],
    output_scripts: &[ScriptBuf],
    fee_rate: FeeRate,
) -> Result<u64, ChainError> {
    let transaction = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: inputs
            .iter()
            .map(|utxo| TxIn {
                previous_output: OutPoint::new(
                    Txid::from_byte_array(utxo.transaction_id),
                    utxo.output_index,
                ),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            })
            .collect(),
        output: output_scripts
            .iter()
            .map(|script_pubkey| TxOut {
                value: Amount::ZERO,
                script_pubkey: script_pubkey.clone(),
            })
            .collect(),
    };
    let satisfaction_weight = inputs.iter().try_fold(0_u64, |total, utxo| {
        total.checked_add(utxo.satisfaction_weight).ok_or_else(|| {
            ChainError::invalid_transaction("Bitcoin satisfaction weight overflowed u64")
        })
    })?;
    let weight = transaction
        .weight()
        .to_wu()
        .checked_add(SEGWIT_MARKER_FLAG_WEIGHT)
        .and_then(|weight| weight.checked_add(satisfaction_weight))
        .ok_or_else(|| {
            ChainError::invalid_transaction("Bitcoin transaction weight overflowed u64")
        })?;
    fee_rate.for_vsize(weight.div_ceil(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_and_grouped_funding_keep_checked_input_total_and_overflow_error() {
        let script = ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([7; 20]));
        let address = crate::Address::from_encoded(
            bitcoin::Address::from_script(&script, Network::Regtest.native())
                .unwrap()
                .to_string(),
        );
        for first in [u64::MAX - 1, u64::MAX] {
            let available = [first, 1]
                .into_iter()
                .enumerate()
                .map(|(index, value)| SpendSource {
                    transaction_id: [index as u8; 32],
                    output_index: 0,
                    value: Satoshi(value),
                    script_pubkey: script.clone().into_bytes(),
                    satisfaction_weight: 109,
                })
                .collect::<Vec<_>>();
            let recipients = vec![Output::from_atomic(address.clone(), Satoshi(1_000))];
            let drain = BuildRequest {
                available: available.clone(),
                recipients: recipients.clone(),
                change_address: address.clone(),
                fee_rate: FeeRate::new(1),
                drain_wallet: true,
            }
            .build(Network::Regtest);
            let grouped = build_grouped(
                Network::Regtest,
                vec![Funding {
                    available,
                    recipients,
                    change_address: address.clone(),
                }],
                FeeRate::new(1),
            );
            if first == u64::MAX - 1 {
                assert!(drain.is_ok());
                assert!(grouped.is_ok());
                continue;
            }
            for result in [drain, grouped] {
                let error = result.unwrap_err();
                assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
                assert_eq!(
                    error.message,
                    "Bitcoin selected input amount overflowed u64"
                );
            }
        }
    }

    #[test]
    fn fee_allocation_charges_sources_in_input_order() {
        let surplus = [4, 0, 9, 7];

        assert_eq!(
            allocate_fee(&surplus, 10).expect("sufficient surplus"),
            [0, 0, 3, 7]
        );
        assert_eq!(
            allocate_fee(&[7, 0, 9, 4], 10).expect("reordered surplus"),
            [0, 0, 6, 4]
        );
    }

    #[test]
    fn zero_fee_preserves_every_sources_surplus() {
        assert_eq!(allocate_fee(&[5, 0, 9], 0).expect("zero fee"), [5, 0, 9]);
        assert!(allocate_fee(&[], 0).expect("empty zero fee").is_empty());
    }

    #[test]
    fn fee_equal_to_total_surplus_leaves_no_change() {
        assert_eq!(allocate_fee(&[5, 0, 9], 14).expect("exact fee"), [0, 0, 0]);
    }

    #[test]
    fn uncovered_fee_returns_insufficient_funds() {
        for (surplus, fee) in [(&[5, 0, 9][..], 15), (&[][..], 1)] {
            let error = allocate_fee(surplus, fee).expect_err("fee exceeds surplus");

            assert_eq!(error.kind, ChainErrorKind::InsufficientFunds);
            assert_eq!(
                error.message,
                "Bitcoin grouped sources cannot cover the network fee"
            );
        }
    }

    #[test]
    fn fee_allocation_handles_u64_extrema_without_summing_surplus() {
        assert_eq!(
            allocate_fee(&[u64::MAX, u64::MAX], u64::MAX).expect("fee fits first source"),
            [0, u64::MAX]
        );
        assert_eq!(
            allocate_fee(&[1, u64::MAX], u64::MAX).expect("fee spans sources"),
            [0, 1]
        );
        assert_eq!(
            allocate_fee(&[0, u64::MAX], u64::MAX).expect("exact maximum fee"),
            [0, 0]
        );
    }
}
