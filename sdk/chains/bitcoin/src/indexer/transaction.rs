use std::collections::BTreeSet;

use bitcoin::ScriptBuf;
use indexing::{
    CanonicalAddress, IndexError, MovementId, NetworkFee, TransactionRef, ValueMovement, WatchId,
};

use crate::{Address, Network, TransactionId};

use super::{
    IndexedOutput, Outpoint, UtxoKey,
    interpreter::{NATIVE_ASSET, ValidatedWatches, invalid_block},
    model::{Input, Output, PreviousOutput, Transaction, address_for_script},
};

pub(super) trait Canonicalize {
    type Output;

    fn canonical(self, scope: &indexing::IndexScope) -> Self::Output;
}

impl Canonicalize for Address {
    type Output = CanonicalAddress;

    fn canonical(self, scope: &indexing::IndexScope) -> Self::Output {
        CanonicalAddress {
            scope: scope.clone(),
            value: self.encoded().to_owned(),
        }
    }
}

impl Canonicalize for TransactionId {
    type Output = TransactionRef;

    fn canonical(self, scope: &indexing::IndexScope) -> Self::Output {
        TransactionRef {
            scope: scope.clone(),
            value: self.to_string(),
        }
    }
}

pub(super) struct InterpretedTransaction {
    pub(super) movements: Vec<ValueMovement>,
    pub(super) fee: Option<NetworkFee>,
    pub(super) watch_ids: Vec<WatchId>,
    pub(super) creates: Vec<IndexedOutput>,
    pub(super) spends: Vec<UtxoKey>,
    pub(super) tracked_spends: Vec<UtxoKey>,
}

impl InterpretedTransaction {
    pub(super) fn from_transaction(
        transaction: &Transaction,
        block_height: indexing::BlockHeight,
        network: Network,
        scope: &indexing::IndexScope,
        watches: &ValidatedWatches,
        all_spent_outpoints: &mut BTreeSet<Outpoint>,
    ) -> Result<InterpretedTransaction, IndexError> {
        let mut facts = TransactionFacts::new(transaction, scope)?;
        for (index, input) in transaction.inputs.iter().enumerate() {
            facts.input(
                input,
                index,
                transaction.coinbase,
                network,
                watches,
                all_spent_outpoints,
            )?;
        }
        for (index, output) in transaction.outputs.iter().enumerate() {
            facts.output(output, index, transaction, block_height, network, watches)?;
        }
        facts.finish(transaction.coinbase)
    }
}

struct TransactionFacts {
    scope: indexing::IndexScope,
    transaction_id: String,
    movements: Vec<ValueMovement>,
    watch_ids: BTreeSet<WatchId>,
    input_total: u64,
    output_total: u64,
    payer: Option<Address>,
    ambiguous_payer: bool,
    creates: Vec<IndexedOutput>,
    spends: Vec<UtxoKey>,
    tracked_spends: Vec<UtxoKey>,
}

impl TransactionFacts {
    fn new(transaction: &Transaction, scope: &indexing::IndexScope) -> Result<Self, IndexError> {
        let movement_capacity = transaction
            .inputs
            .len()
            .checked_add(transaction.outputs.len())
            .ok_or_else(|| invalid_block("Bitcoin transaction movement count overflowed"))?;
        Ok(Self {
            scope: scope.clone(),
            transaction_id: transaction.id.to_string(),
            movements: Vec::with_capacity(movement_capacity),
            watch_ids: BTreeSet::new(),
            input_total: 0,
            output_total: 0,
            payer: None,
            ambiguous_payer: false,
            creates: Vec::new(),
            spends: Vec::new(),
            tracked_spends: Vec::new(),
        })
    }

    fn input(
        &mut self,
        input: &Input,
        index: usize,
        coinbase: bool,
        network: Network,
        watches: &ValidatedWatches,
        all_spent_outpoints: &mut BTreeSet<Outpoint>,
    ) -> Result<(), IndexError> {
        match (&input.previous_output, coinbase) {
            (None, true) => Ok(()),
            (None, false) => Err(invalid_block(
                "non-coinbase Bitcoin transaction has an unresolved input",
            )),
            (Some(_), true) => Err(invalid_block(
                "Bitcoin coinbase transaction contains a resolved normal input",
            )),
            (Some(previous), false) => {
                self.previous_output(previous, index, network, watches, all_spent_outpoints)
            }
        }
    }

    fn previous_output(
        &mut self,
        previous: &PreviousOutput,
        index: usize,
        network: Network,
        watches: &ValidatedWatches,
        all_spent_outpoints: &mut BTreeSet<Outpoint>,
    ) -> Result<(), IndexError> {
        if !all_spent_outpoints.insert(previous.outpoint) {
            return Err(invalid_block(
                "Bitcoin block spends the same outpoint more than once",
            ));
        }
        self.input_total = self
            .input_total
            .checked_add(previous.value.0)
            .ok_or_else(|| invalid_block("Bitcoin transaction input value overflowed u64"))?;
        self.movements.push(ValueMovement::Input {
            id: MovementId(format!("{}:vin:{index}", self.transaction_id)),
            asset: (*NATIVE_ASSET).clone(),
            amount: base::Decimal::from(previous.value.0),
            owner: previous
                .address
                .clone()
                .map(|address| address.canonical(&self.scope)),
        });
        self.record_payer(previous.address.as_ref());
        self.record_spend(previous, network, watches)
    }

    fn record_payer(&mut self, address: Option<&Address>) {
        match (&self.payer, address) {
            (None, Some(address)) if !self.ambiguous_payer => self.payer = Some(address.clone()),
            (Some(current), Some(address)) if current == address => {}
            _ => self.ambiguous_payer = true,
        }
    }

    fn record_spend(
        &mut self,
        previous: &PreviousOutput,
        network: Network,
        watches: &ValidatedWatches,
    ) -> Result<(), IndexError> {
        let Some(address) = previous.address.as_ref() else {
            return Ok(());
        };
        let key = UtxoKey {
            address: address.clone(),
            outpoint: previous.outpoint,
        };
        if let Some(ids) = watches.active_addresses.get(address.encoded()) {
            self.watch_ids.extend(ids.iter().cloned());
            self.spends.push(key);
            return Ok(());
        }
        let script = address.script_pubkey_for_network(network).map_err(|_| {
            invalid_block("Bitcoin compact prevout address cannot produce a script")
        })?;
        if script.is_p2wpkh() || script.is_p2tr() {
            self.tracked_spends.push(key);
        }
        Ok(())
    }

    fn output(
        &mut self,
        output: &Output,
        index: usize,
        transaction: &Transaction,
        block_height: indexing::BlockHeight,
        network: Network,
        watches: &ValidatedWatches,
    ) -> Result<(), IndexError> {
        self.output_total = self
            .output_total
            .checked_add(output.value.0)
            .ok_or_else(|| invalid_block("Bitcoin transaction output value overflowed u64"))?;
        let script = ScriptBuf::from_bytes(output.script_pubkey.clone());
        let address = address_for_script(&script, network);
        self.movements.push(ValueMovement::Output {
            id: MovementId(format!("{}:vout:{index}", self.transaction_id)),
            asset: (*NATIVE_ASSET).clone(),
            amount: base::Decimal::from(output.value.0),
            owner: address
                .clone()
                .map(|address| address.canonical(&self.scope)),
        });
        let Some(address) = address else {
            return Ok(());
        };
        let Some(ids) = watches.active_addresses.get(address.encoded()) else {
            return Ok(());
        };
        self.watch_ids.extend(ids.iter().cloned());
        let output_index =
            u32::try_from(index).map_err(|_| invalid_block("Bitcoin output index exceeds u32"))?;
        self.creates.push(IndexedOutput {
            outpoint: Outpoint {
                transaction_id: transaction.id,
                output_index,
            },
            value: output.value,
            script_pubkey: output.script_pubkey.clone(),
            address,
            created_height: block_height,
            coinbase: transaction.coinbase,
        });
        Ok(())
    }

    fn finish(self, coinbase: bool) -> Result<InterpretedTransaction, IndexError> {
        let fee = if coinbase {
            None
        } else {
            let amount = self
                .input_total
                .checked_sub(self.output_total)
                .ok_or_else(|| {
                    invalid_block("Bitcoin transaction outputs exceed its resolved inputs")
                })?;
            Some(NetworkFee {
                asset: (*NATIVE_ASSET).clone(),
                amount: base::Decimal::from(amount),
                payer: (!self.ambiguous_payer)
                    .then_some(self.payer)
                    .flatten()
                    .map(|address| address.canonical(&self.scope)),
            })
        };
        Ok(InterpretedTransaction {
            movements: self.movements,
            fee,
            watch_ids: self.watch_ids.into_iter().collect(),
            creates: self.creates,
            spends: self.spends,
            tracked_spends: self.tracked_spends,
        })
    }
}
