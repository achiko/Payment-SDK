use std::collections::BTreeSet;

use bitcoin::ScriptBuf;
use indexing::{
    CanonicalAddress, IndexError, MovementId, NetworkFee, OutputId, OutputKey, TransactionRef,
    ValueMovement,
};

use crate::{Address, Network, TransactionId};

use super::{
    IndexedOutput, Outpoint, UtxoKey,
    interpreter::{NATIVE_ASSET, ValidatedAddresses, invalid_block},
    model::{Input, Output, PreviousOutput, Transaction},
};

pub(super) trait Canonicalize {
    type Output;

    fn canonical(&self, scope: &indexing::IndexScope) -> Self::Output;
}

impl Canonicalize for Address {
    type Output = CanonicalAddress;

    fn canonical(&self, scope: &indexing::IndexScope) -> Self::Output {
        CanonicalAddress {
            scope: scope.clone(),
            value: self.encoded().to_owned(),
        }
    }
}

impl Canonicalize for TransactionId {
    type Output = TransactionRef;

    fn canonical(&self, scope: &indexing::IndexScope) -> Self::Output {
        TransactionRef {
            scope: scope.clone(),
            value: self.to_string(),
        }
    }
}

impl Canonicalize for UtxoKey {
    type Output = OutputKey;

    fn canonical(&self, scope: &indexing::IndexScope) -> Self::Output {
        OutputKey {
            address: self.address.canonical(scope),
            output: OutputId {
                transaction: self.outpoint.transaction_id.canonical(scope),
                index: self.outpoint.output_index,
            },
        }
    }
}

impl Canonicalize for IndexedOutput {
    type Output = indexing::IndexedOutput;

    fn canonical(&self, scope: &indexing::IndexScope) -> Self::Output {
        indexing::IndexedOutput {
            id: OutputId {
                transaction: self.outpoint.transaction_id.canonical(scope),
                index: self.outpoint.output_index,
            },
            address: self.address.canonical(scope),
            asset: (*NATIVE_ASSET).clone(),
            amount: base::Decimal::from(self.value.0),
            evidence: self.script_pubkey.clone(),
            created_at: self.created_height,
            coinbase: self.coinbase,
        }
    }
}

pub(super) struct InterpretedTransaction {
    pub(super) movements: Vec<ValueMovement>,
    pub(super) fee: Option<NetworkFee>,
    pub(super) relevant: bool,
    pub(super) creates: Vec<IndexedOutput>,
    pub(super) spends: Vec<UtxoKey>,
    pub(super) tracked_spends: Vec<UtxoKey>,
}

impl Transaction {
    pub(super) fn interpret(
        &self,
        block_height: indexing::BlockHeight,
        network: Network,
        scope: &indexing::IndexScope,
        addresses: &ValidatedAddresses,
        all_spent_outpoints: &mut BTreeSet<Outpoint>,
    ) -> Result<InterpretedTransaction, IndexError> {
        let mut facts = TransactionFacts::new(self, block_height, network, scope, addresses)?;
        for (index, input) in self.inputs.iter().enumerate() {
            facts.input(input, index, all_spent_outpoints)?;
        }
        for (index, output) in self.outputs.iter().enumerate() {
            facts.output(output, index)?;
        }
        facts.finish()
    }
}

struct TransactionFacts<'a> {
    scope: &'a indexing::IndexScope,
    addresses: &'a ValidatedAddresses,
    transaction_id: TransactionId,
    block_height: indexing::BlockHeight,
    network: Network,
    coinbase: bool,
    movements: Vec<ValueMovement>,
    relevant: bool,
    input_total: u64,
    output_total: u64,
    payer: Option<Address>,
    ambiguous_payer: bool,
    creates: Vec<IndexedOutput>,
    spends: Vec<UtxoKey>,
    tracked_spends: Vec<UtxoKey>,
}

impl<'a> TransactionFacts<'a> {
    fn new(
        transaction: &Transaction,
        block_height: indexing::BlockHeight,
        network: Network,
        scope: &'a indexing::IndexScope,
        addresses: &'a ValidatedAddresses,
    ) -> Result<Self, IndexError> {
        let movement_capacity = transaction
            .inputs
            .len()
            .checked_add(transaction.outputs.len())
            .ok_or_else(|| invalid_block("Bitcoin transaction movement count overflowed"))?;
        Ok(Self {
            scope,
            addresses,
            transaction_id: transaction.id,
            block_height,
            network,
            coinbase: transaction.coinbase,
            movements: Vec::with_capacity(movement_capacity),
            relevant: false,
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
        all_spent_outpoints: &mut BTreeSet<Outpoint>,
    ) -> Result<(), IndexError> {
        match (&input.previous_output, self.coinbase) {
            (None, true) => Ok(()),
            (None, false) => Err(invalid_block(
                "non-coinbase Bitcoin transaction has an unresolved input",
            )),
            (Some(_), true) => Err(invalid_block(
                "Bitcoin coinbase transaction contains a resolved normal input",
            )),
            (Some(previous), false) => self.previous_output(previous, index, all_spent_outpoints),
        }
    }

    fn previous_output(
        &mut self,
        previous: &PreviousOutput,
        index: usize,
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
                .as_ref()
                .map(|address| address.canonical(self.scope)),
        });
        self.record_payer(previous.address.as_ref());
        self.record_spend(previous)
    }

    fn record_payer(&mut self, address: Option<&Address>) {
        match (&self.payer, address) {
            (None, Some(address)) if !self.ambiguous_payer => self.payer = Some(address.clone()),
            (Some(current), Some(address)) if current == address => {}
            _ => self.ambiguous_payer = true,
        }
    }

    fn record_spend(&mut self, previous: &PreviousOutput) -> Result<(), IndexError> {
        let Some(address) = previous.address.as_ref() else {
            return Ok(());
        };
        let key = UtxoKey {
            address: address.clone(),
            outpoint: previous.outpoint,
        };
        if self.addresses.contains(address) {
            self.relevant = true;
            self.spends.push(key);
            return Ok(());
        }
        let script = address
            .script_pubkey_for_network(self.network)
            .map_err(|_| {
                invalid_block("Bitcoin compact prevout address cannot produce a script")
            })?;
        if script.is_p2wpkh() || script.is_p2tr() {
            self.tracked_spends.push(key);
        }
        Ok(())
    }

    fn output(&mut self, output: &Output, index: usize) -> Result<(), IndexError> {
        self.output_total = self
            .output_total
            .checked_add(output.value.0)
            .ok_or_else(|| invalid_block("Bitcoin transaction output value overflowed u64"))?;
        let script = ScriptBuf::from_bytes(output.script_pubkey.clone());
        let address = Address::from_script_for_network(&script, self.network);
        self.movements.push(ValueMovement::Output {
            id: MovementId(format!("{}:vout:{index}", self.transaction_id)),
            asset: (*NATIVE_ASSET).clone(),
            amount: base::Decimal::from(output.value.0),
            owner: address
                .as_ref()
                .map(|address| address.canonical(self.scope)),
        });
        let Some(address) = address else {
            return Ok(());
        };
        if !self.addresses.contains(&address) {
            return Ok(());
        }
        self.relevant = true;
        let output_index =
            u32::try_from(index).map_err(|_| invalid_block("Bitcoin output index exceeds u32"))?;
        self.creates.push(IndexedOutput {
            outpoint: Outpoint {
                transaction_id: self.transaction_id,
                output_index,
            },
            value: output.value,
            script_pubkey: output.script_pubkey.clone(),
            address,
            created_height: self.block_height,
            coinbase: self.coinbase,
        });
        Ok(())
    }

    fn finish(self) -> Result<InterpretedTransaction, IndexError> {
        let fee = if self.coinbase {
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
                    .map(|address| address.canonical(self.scope)),
            })
        };
        Ok(InterpretedTransaction {
            movements: self.movements,
            fee,
            relevant: self.relevant,
            creates: self.creates,
            spends: self.spends,
            tracked_spends: self.tracked_spends,
        })
    }
}
