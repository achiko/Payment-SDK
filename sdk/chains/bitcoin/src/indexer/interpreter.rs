use std::collections::{BTreeMap, BTreeSet};

use bitcoin::ScriptBuf;
use chain_identity::{AssetId, AtomicAmount, CanonicalAddress, CanonicalTransactionId, ChainId};
use indexing::{
    BlockInterpreter, IndexError, IndexErrorKind, IndexScope, InterpretedBlock, MovementId,
    MovementKind, NetworkFee, ObservationDraft, ObservationDraftStatus, RawBlockData,
    ValueMovement, WatchId, WatchSelector, WatchTarget,
};

use crate::{BitcoinAddress, BitcoinNetwork, BitcoinTransactionId};

use super::{
    BitcoinBlock, BitcoinIndexRecordCodec, BitcoinIndexedOutput, BitcoinOutPoint, BitcoinUndo,
    BitcoinUtxoKey, BitcoinUtxoProjection, BitcoinWatchTarget,
    model::{ParsedTransaction, address_for_script, parse_block},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinBlockInterpreter {
    scope: IndexScope,
    network: BitcoinNetwork,
}

impl BitcoinBlockInterpreter {
    pub fn new(scope: IndexScope, network: BitcoinNetwork) -> Result<Self, IndexError> {
        if scope.chain != bitcoin_chain() {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Bitcoin interpreter scope must use the bitcoin chain ID",
                false,
            ));
        }
        if scope.network != network.canonical_name() {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Bitcoin interpreter scope network does not match configuration",
                false,
            ));
        }
        Ok(Self { scope, network })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    #[must_use]
    pub const fn network(&self) -> BitcoinNetwork {
        self.network
    }

    fn validated_watches(
        &self,
        watches: &[WatchTarget<BitcoinWatchTarget>],
        height: indexing::BlockHeight,
    ) -> Result<ValidatedWatches, IndexError> {
        let mut validated = ValidatedWatches::default();
        for watch in watches {
            if watch.scope != self.scope {
                return Err(invalid_watch("Bitcoin watch belongs to a different scope"));
            }
            match (&watch.target, &watch.selector) {
                (BitcoinWatchTarget::Address(address), WatchSelector::Address(selector)) => {
                    let canonical = BitcoinAddress::parse_for_network(&address.0, self.network)
                        .map_err(|_| {
                            invalid_watch("Bitcoin watch address is invalid or wrong-network")
                        })?;
                    let script =
                        canonical
                            .script_pubkey_for_network(self.network)
                            .map_err(|_| {
                                invalid_watch("Bitcoin watch address cannot produce a script")
                            })?;
                    if !script.is_p2wpkh() && !script.is_p2tr() {
                        return Err(invalid_watch(
                            "Bitcoin v1 address watches support P2WPKH and P2TR only",
                        ));
                    }
                    if selector.chain != bitcoin_chain() || selector.value != canonical.0 {
                        return Err(invalid_watch(
                            "Bitcoin watch target does not match its canonical address selector",
                        ));
                    }
                    if watch.is_active_at(height) {
                        validated
                            .active_addresses
                            .entry(canonical.0)
                            .or_default()
                            .insert(watch.id.clone());
                    }
                }
                (
                    BitcoinWatchTarget::Transaction(transaction_id),
                    WatchSelector::Transaction(selector),
                ) => {
                    if selector.chain != bitcoin_chain()
                        || selector.value != transaction_id.to_string()
                    {
                        return Err(invalid_watch(
                            "Bitcoin watch target does not match its canonical transaction selector",
                        ));
                    }
                    if watch.is_active_at(height) {
                        validated
                            .transactions
                            .entry(*transaction_id)
                            .or_default()
                            .insert(watch.id.clone());
                    }
                }
                _ => {
                    return Err(invalid_watch(
                        "Bitcoin watch target kind does not match its selector",
                    ));
                }
            }
        }
        Ok(validated)
    }
}

impl BlockInterpreter for BitcoinBlockInterpreter {
    type Block = BitcoinBlock;
    type Target = BitcoinWatchTarget;
    type Undo = BitcoinUndo;

    fn inspect(
        &self,
        block: &Self::Block,
        watches: &[WatchTarget<Self::Target>],
    ) -> Result<InterpretedBlock<Self::Undo>, IndexError> {
        let parsed = parse_block(
            &block.raw_block,
            Some(block.reference.height),
            Some(&block.reference.hash),
            self.network,
        )
        .map_err(invalid_block)?;
        if parsed.reference != block.reference {
            return Err(invalid_block(
                "retained Bitcoin block reference does not match its raw payload",
            ));
        }
        let observed_at = block
            .reference
            .timestamp
            .ok_or_else(|| invalid_block("Bitcoin block timestamp is unavailable"))?;
        let watches = self.validated_watches(watches, block.reference.height)?;

        let mut drafts = Vec::new();
        let mut creates = BTreeMap::<BitcoinOutPoint, BitcoinIndexedOutput>::new();
        let mut spends = BTreeMap::<BitcoinOutPoint, BitcoinUtxoKey>::new();
        let mut conditional_spends = BTreeMap::<BitcoinOutPoint, BitcoinUtxoKey>::new();
        let mut all_spent_outpoints = BTreeSet::new();

        for transaction in &parsed.transactions {
            let interpreted = interpret_transaction(
                transaction,
                block.reference.height,
                self.network,
                &watches,
                &mut all_spent_outpoints,
            )?;
            for output in interpreted.creates {
                if creates.insert(output.outpoint, output).is_some() {
                    return Err(invalid_block(
                        "Bitcoin block creates a duplicate watched outpoint",
                    ));
                }
            }
            for output in interpreted.spends {
                if creates.remove(&output.outpoint).is_some() {
                    // A watched output created and spent within this block did
                    // not exist before or after the block. Movements remain,
                    // but canonical projection and undo must contain no effect.
                    continue;
                }
                if spends.insert(output.outpoint, output).is_some() {
                    return Err(invalid_block(
                        "Bitcoin block spends a watched outpoint more than once",
                    ));
                }
            }
            for output in interpreted.conditional_spends {
                if creates.remove(&output.outpoint).is_some() {
                    continue;
                }
                if conditional_spends.insert(output.outpoint, output).is_some() {
                    return Err(invalid_block(
                        "Bitcoin block conditionally spends a watched outpoint more than once",
                    ));
                }
            }
            if !interpreted.watch_ids.is_empty() {
                drafts.push(ObservationDraft {
                    scope: self.scope.clone(),
                    transaction_id: canonical_transaction(transaction.id),
                    status: ObservationDraftStatus::Included,
                    movements: interpreted.movements,
                    fee: interpreted.fee,
                    watch_ids: interpreted.watch_ids,
                    first_seen_at: observed_at,
                    observed_at,
                });
            }
        }

        let creates: Vec<_> = creates.into_values().collect();
        let spends: Vec<_> = spends.into_values().collect();
        let conditional_spends: Vec<_> = conditional_spends.into_values().collect();
        let chain_projection = BitcoinUtxoProjection {
            creates: creates.clone(),
            spends: spends.clone(),
            conditional_spends: conditional_spends.clone(),
        };
        let projection = BitcoinIndexRecordCodec::projection_batch(&chain_projection)?;
        Ok(InterpretedBlock {
            block: block.reference.clone(),
            drafts,
            projection,
            undo: BitcoinUndo {
                remove_created: creates.iter().map(utxo_key).collect(),
                remove_spent_markers: spends.iter().cloned().chain(conditional_spends).collect(),
            },
            raw: RawBlockData {
                block: block.raw_block.clone(),
                receipts: Vec::new(),
            },
        })
    }
}

fn utxo_key(output: &BitcoinIndexedOutput) -> BitcoinUtxoKey {
    BitcoinUtxoKey {
        address: output.address.clone(),
        outpoint: output.outpoint,
    }
}

#[derive(Default)]
struct ValidatedWatches {
    active_addresses: BTreeMap<String, BTreeSet<WatchId>>,
    transactions: BTreeMap<BitcoinTransactionId, BTreeSet<WatchId>>,
}

struct InterpretedTransaction {
    movements: Vec<ValueMovement>,
    fee: Option<NetworkFee>,
    watch_ids: Vec<WatchId>,
    creates: Vec<BitcoinIndexedOutput>,
    spends: Vec<BitcoinUtxoKey>,
    conditional_spends: Vec<BitcoinUtxoKey>,
}

fn interpret_transaction(
    transaction: &ParsedTransaction,
    block_height: indexing::BlockHeight,
    network: BitcoinNetwork,
    watches: &ValidatedWatches,
    all_spent_outpoints: &mut BTreeSet<BitcoinOutPoint>,
) -> Result<InterpretedTransaction, IndexError> {
    let transaction_id = transaction.id.to_string();
    let mut movements = Vec::with_capacity(
        transaction
            .inputs
            .len()
            .checked_add(transaction.outputs.len())
            .ok_or_else(|| invalid_block("Bitcoin transaction movement count overflowed"))?,
    );
    let mut watch_ids = watches
        .transactions
        .get(&transaction.id)
        .cloned()
        .unwrap_or_default();
    let mut input_total = 0_u64;
    let mut output_total = 0_u64;
    let mut payer: Option<BitcoinAddress> = None;
    let mut ambiguous_payer = false;
    let mut spends = Vec::new();
    let mut conditional_spends = Vec::new();

    for (index, input) in transaction.inputs.iter().enumerate() {
        let Some(previous) = input.previous_output.as_ref() else {
            if !transaction.coinbase {
                return Err(invalid_block(
                    "non-coinbase Bitcoin transaction has an unresolved input",
                ));
            }
            continue;
        };
        if !all_spent_outpoints.insert(previous.outpoint) {
            return Err(invalid_block(
                "Bitcoin block spends the same outpoint more than once",
            ));
        }
        input_total = input_total
            .checked_add(previous.value.0)
            .ok_or_else(|| invalid_block("Bitcoin transaction input value overflowed u64"))?;
        let from = previous
            .address
            .as_ref()
            .map(|address| canonical_address(address.clone()));
        movements.push(ValueMovement {
            id: MovementId(format!("{transaction_id}:vin:{index}")),
            asset: native_asset(),
            amount: atomic(previous.value.0),
            from,
            to: None,
            kind: MovementKind::Input,
        });
        match (&payer, &previous.address) {
            (None, Some(address)) if !ambiguous_payer => payer = Some(address.clone()),
            (Some(current), Some(address)) if current == address => {}
            _ => ambiguous_payer = true,
        }
        if let Some(address) = &previous.address {
            if let Some(ids) = watches.active_addresses.get(&address.0) {
                watch_ids.extend(ids.iter().cloned());
                spends.push(BitcoinUtxoKey {
                    address: address.clone(),
                    outpoint: previous.outpoint,
                });
            } else {
                let script = address.script_pubkey_for_network(network).map_err(|_| {
                    invalid_block("Bitcoin compact prevout address cannot produce a script")
                })?;
                if script.is_p2wpkh() || script.is_p2tr() {
                    conditional_spends.push(BitcoinUtxoKey {
                        address: address.clone(),
                        outpoint: previous.outpoint,
                    });
                }
            }
        }
    }

    let mut creates = Vec::new();
    for (index, output) in transaction.outputs.iter().enumerate() {
        output_total = output_total
            .checked_add(output.value.0)
            .ok_or_else(|| invalid_block("Bitcoin transaction output value overflowed u64"))?;
        let script = ScriptBuf::from_bytes(output.script_pubkey.clone());
        let address = address_for_script(&script, network);
        movements.push(ValueMovement {
            id: MovementId(format!("{transaction_id}:vout:{index}")),
            asset: native_asset(),
            amount: atomic(output.value.0),
            from: None,
            to: address.clone().map(canonical_address),
            kind: MovementKind::Output,
        });
        if let Some(address) = address {
            if let Some(ids) = watches.active_addresses.get(&address.0) {
                watch_ids.extend(ids.iter().cloned());
                let output_index = u32::try_from(index)
                    .map_err(|_| invalid_block("Bitcoin output index exceeds u32"))?;
                creates.push(BitcoinIndexedOutput {
                    outpoint: BitcoinOutPoint {
                        transaction_id: transaction.id,
                        output_index,
                    },
                    value: output.value,
                    script_pubkey: output.script_pubkey.clone(),
                    address,
                    created_height: block_height,
                    coinbase: transaction.coinbase,
                });
            }
        }
    }

    let fee = if transaction.coinbase {
        if transaction
            .inputs
            .iter()
            .any(|input| input.previous_output.is_some())
        {
            return Err(invalid_block(
                "Bitcoin coinbase transaction contains a resolved normal input",
            ));
        }
        None
    } else {
        let amount = input_total.checked_sub(output_total).ok_or_else(|| {
            invalid_block("Bitcoin transaction outputs exceed its resolved inputs")
        })?;
        Some(NetworkFee {
            asset: native_asset(),
            amount: atomic(amount),
            payer: (!ambiguous_payer)
                .then_some(payer)
                .flatten()
                .map(canonical_address),
        })
    };

    Ok(InterpretedTransaction {
        movements,
        fee,
        watch_ids: watch_ids.into_iter().collect(),
        creates,
        spends,
        conditional_spends,
    })
}

fn atomic(value: u64) -> AtomicAmount {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&value.to_be_bytes());
    AtomicAmount(bytes)
}

fn native_asset() -> AssetId {
    AssetId {
        chain: bitcoin_chain(),
        asset: "native".to_owned(),
    }
}

fn canonical_address(address: BitcoinAddress) -> CanonicalAddress {
    CanonicalAddress {
        chain: bitcoin_chain(),
        value: address.0,
    }
}

fn canonical_transaction(transaction: BitcoinTransactionId) -> CanonicalTransactionId {
    CanonicalTransactionId {
        chain: bitcoin_chain(),
        value: transaction.to_string(),
    }
}

fn bitcoin_chain() -> ChainId {
    ChainId("bitcoin".to_owned())
}

fn invalid_watch(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidWatch, message, false)
}

fn invalid_block(message: impl ToString) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, message.to_string(), false)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bitcoin::{
        Address, Amount, CompressedPublicKey, OutPoint, PublicKey, ScriptBuf, Sequence,
        Transaction, TxIn, TxOut, Txid, Witness, XOnlyPublicKey, absolute, consensus, hashes::Hash,
        hex::DisplayHex, secp256k1::Secp256k1, transaction::Version,
    };
    use indexing::{BlockHash, BlockHeight, MovementKind, ProjectionMutation, WatchSelector};
    use serde_json::{Number, Value, json};

    use super::*;

    #[derive(Clone)]
    struct PreviousEvidence {
        value: u64,
        script: ScriptBuf,
        height: u64,
        coinbase: bool,
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: bitcoin_chain(),
            network: "regtest".to_owned(),
        }
    }

    fn p2wpkh_address(prefix: u8) -> Address {
        let public_key = PublicKey::from_slice(&[
            prefix, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        Address::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            bitcoin::Network::Regtest,
        )
    }

    fn p2tr_address() -> Address {
        let key = XOnlyPublicKey::from_slice(&[
            0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
            0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b,
            0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test x-only key must parse");
        Address::p2tr(
            &Secp256k1::verification_only(),
            key,
            None,
            bitcoin::Network::Regtest,
        )
    }

    fn btc_number(satoshis: u64) -> Value {
        let whole = satoshis / 100_000_000;
        let remainder = satoshis % 100_000_000;
        let lexical = if remainder == 0 {
            whole.to_string()
        } else {
            format!("{whole}.{remainder:08}")
                .trim_end_matches('0')
                .to_owned()
        };
        Value::Number(Number::from_str(&lexical).expect("test BTC number must encode"))
    }

    fn transaction_json(transaction: &Transaction, previous: &[Option<PreviousEvidence>]) -> Value {
        assert_eq!(transaction.input.len(), previous.len());
        let inputs: Vec<_> = transaction
            .input
            .iter()
            .zip(previous)
            .map(|(input, previous)| match previous {
                None => json!({"coinbase": "01"}),
                Some(previous) => json!({
                    "txid": input.previous_output.txid.to_string(),
                    "vout": input.previous_output.vout,
                    "prevout": {
                        "generated": previous.coinbase,
                        "height": previous.height,
                        "value": btc_number(previous.value),
                        "scriptPubKey": {
                            "hex": previous.script.as_bytes().to_lower_hex_string()
                        }
                    }
                }),
            })
            .collect();
        let outputs: Vec<_> = transaction
            .output
            .iter()
            .enumerate()
            .map(|(index, output)| {
                json!({
                    "value": btc_number(output.value.to_sat()),
                    "n": index,
                    "scriptPubKey": {
                        "hex": output.script_pubkey.as_bytes().to_lower_hex_string()
                    }
                })
            })
            .collect();
        json!({
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "vin": inputs,
            "vout": outputs
        })
    }

    fn block(transactions: Vec<Value>) -> BitcoinBlock {
        let native_hash = bitcoin::BlockHash::from_byte_array([0xaa; 32]);
        let parent = bitcoin::BlockHash::from_byte_array([0xbb; 32]);
        let raw_block = serde_json::to_vec(&json!({
            "hash": native_hash.to_string(),
            "height": 10,
            "previousblockhash": parent.to_string(),
            "time": 100,
            "nTx": transactions.len(),
            "tx": transactions
        }))
        .expect("test block JSON must encode");
        BitcoinBlock {
            reference: indexing::BlockRef {
                height: BlockHeight(10),
                hash: BlockHash(native_hash.to_byte_array().to_vec()),
                parent_hash: Some(BlockHash(parent.to_byte_array().to_vec())),
                timestamp: Some(100),
            },
            raw_block,
        }
    }

    fn coinbase(output: TxOut) -> Transaction {
        Transaction {
            version: Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![output],
        }
    }

    fn address_watch(id: &str, address: &Address) -> WatchTarget<BitcoinWatchTarget> {
        let address = BitcoinAddress(address.to_string());
        WatchTarget {
            id: WatchId(id.to_owned()),
            scope: scope(),
            selector: WatchSelector::Address(canonical_address(address.clone())),
            target: BitcoinWatchTarget::Address(address),
            idempotency_key: format!("{id}-key"),
            start_height: BlockHeight(0),
            registered_at: None,
            inactive_from: None,
        }
    }

    fn transaction_watch(id: BitcoinTransactionId) -> WatchTarget<BitcoinWatchTarget> {
        WatchTarget {
            id: WatchId("watch-tx".to_owned()),
            scope: scope(),
            selector: WatchSelector::Transaction(canonical_transaction(id)),
            target: BitcoinWatchTarget::Transaction(id),
            idempotency_key: "tx-key".to_owned(),
            start_height: BlockHeight(0),
            registered_at: None,
            inactive_from: None,
        }
    }

    #[test]
    fn same_block_spend_resolves_locally_and_nets_projection_while_emitting_movements() {
        let source = p2wpkh_address(0x02);
        let destination = p2tr_address();
        let funding = coinbase(TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: source.script_pubkey(),
        });
        let spending = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(funding.compute_txid(), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(49_000),
                script_pubkey: destination.script_pubkey(),
            }],
        };
        let block = block(vec![transaction_json(&funding, &[None]), {
            let mut value = transaction_json(
                &spending,
                &[Some(PreviousEvidence {
                    value: 50_000,
                    script: source.script_pubkey(),
                    height: 10,
                    coinbase: true,
                })],
            );
            value["vin"][0]
                .as_object_mut()
                .expect("test input must be an object")
                .remove("prevout");
            value
        }]);

        let interpreted = BitcoinBlockInterpreter::new(scope(), BitcoinNetwork::Regtest)
            .expect("scope must be valid")
            .inspect(
                &block,
                &[
                    address_watch("watch-source", &source),
                    address_watch("watch-destination", &destination),
                ],
            )
            .expect("valid watched block must interpret");

        assert_eq!(interpreted.drafts.len(), 2);
        let spend = &interpreted.drafts[1];
        assert_eq!(spend.movements.len(), 2);
        assert_eq!(spend.movements[0].kind, MovementKind::Input);
        assert_eq!(spend.movements[1].kind, MovementKind::Output);
        assert_eq!(
            spend.movements[0].id,
            MovementId(format!("{}:vin:0", spending.compute_txid()))
        );
        assert_eq!(
            spend
                .fee
                .as_ref()
                .expect("normal transaction has a fee")
                .amount,
            atomic(1_000)
        );
        assert_eq!(interpreted.projection.mutations.len(), 1);
        assert!(matches!(
            interpreted.projection.mutations[0],
            ProjectionMutation::Put { .. }
        ));
        assert_eq!(interpreted.undo.remove_created.len(), 1);
        assert_eq!(interpreted.undo.remove_spent_markers.len(), 0);
    }

    #[test]
    fn transaction_watch_keeps_non_address_output_and_ambiguous_fee_payer() {
        let first = p2wpkh_address(0x02);
        let second = p2wpkh_address(0x03);
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([1; 32]), 0),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([2; 32]), 1),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
            ],
            output: vec![TxOut {
                value: Amount::from_sat(19_000),
                script_pubkey: ScriptBuf::new_op_return([]),
            }],
        };
        let block = block(vec![transaction_json(
            &transaction,
            &[
                Some(PreviousEvidence {
                    value: 10_000,
                    script: first.script_pubkey(),
                    height: 9,
                    coinbase: false,
                }),
                Some(PreviousEvidence {
                    value: 10_000,
                    script: second.script_pubkey(),
                    height: 9,
                    coinbase: false,
                }),
            ],
        )]);
        let id = BitcoinTransactionId::from(transaction.compute_txid());

        let interpreted = BitcoinBlockInterpreter::new(scope(), BitcoinNetwork::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[transaction_watch(id)])
            .expect("valid watched transaction must interpret");

        let draft = &interpreted.drafts[0];
        assert_eq!(draft.movements.len(), 3);
        assert_eq!(draft.movements[2].to, None);
        assert_eq!(draft.fee.as_ref().expect("fee must exist").payer, None);
        assert_eq!(interpreted.projection.mutations.len(), 2);
        assert!(
            interpreted
                .projection
                .mutations
                .iter()
                .all(|mutation| { matches!(mutation, ProjectionMutation::PutIfPresent { .. }) })
        );
        assert_eq!(interpreted.undo.remove_spent_markers.len(), 2);
    }

    #[test]
    fn active_address_spend_uses_an_unconditional_marker() {
        let source = p2wpkh_address(0x02);
        let destination = p2tr_address();
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([9; 32]), 1),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(9_000),
                script_pubkey: destination.script_pubkey(),
            }],
        };
        let block = block(vec![transaction_json(
            &transaction,
            &[Some(PreviousEvidence {
                value: 10_000,
                script: source.script_pubkey(),
                height: 4,
                coinbase: false,
            })],
        )]);

        let interpreted = BitcoinBlockInterpreter::new(scope(), BitcoinNetwork::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[address_watch("active-source", &source)])
            .expect("active watch spend must interpret");

        assert_eq!(interpreted.drafts.len(), 1);
        assert_eq!(interpreted.projection.mutations.len(), 1);
        assert!(matches!(
            interpreted.projection.mutations[0],
            ProjectionMutation::Put { .. }
        ));
        assert!(interpreted.undo.remove_created.is_empty());
        assert_eq!(interpreted.undo.remove_spent_markers.len(), 1);
    }

    #[test]
    fn inactive_or_absent_address_watch_uses_an_input_bounded_conditional_marker() {
        let source = p2wpkh_address(0x02);
        let destination = p2tr_address();
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([9; 32]), 1),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(9_000),
                script_pubkey: destination.script_pubkey(),
            }],
        };
        let block = block(vec![transaction_json(
            &transaction,
            &[Some(PreviousEvidence {
                value: 10_000,
                script: source.script_pubkey(),
                height: 4,
                coinbase: false,
            })],
        )]);
        let mut watch = address_watch("inactive-source", &source);
        watch.inactive_from = Some(BlockHeight(10));

        let interpreted = BitcoinBlockInterpreter::new(scope(), BitcoinNetwork::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[watch])
            .expect("inactive projection interest must remain valid");

        assert!(interpreted.drafts.is_empty());
        assert_eq!(interpreted.projection.mutations.len(), 1);
        let ProjectionMutation::PutIfPresent {
            required_key, key, ..
        } = &interpreted.projection.mutations[0]
        else {
            panic!("inactive spend must use a conditional marker");
        };
        assert!(matches!(
            BitcoinIndexRecordCodec::decode_projection_key(required_key)
                .expect("required creation key must decode"),
            super::super::BitcoinProjectionKey::Utxo { .. }
        ));
        assert!(matches!(
            BitcoinIndexRecordCodec::decode_projection_key(key)
                .expect("conditional marker key must decode"),
            super::super::BitcoinProjectionKey::SpentMarker { .. }
        ));
        assert!(interpreted.undo.remove_created.is_empty());
        assert_eq!(interpreted.undo.remove_spent_markers.len(), 1);

        let without_watch = BitcoinBlockInterpreter::new(scope(), BitcoinNetwork::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[])
            .expect("unwatched supported input must interpret");
        assert!(without_watch.drafts.is_empty());
        assert!(matches!(
            without_watch.projection.mutations.as_slice(),
            [ProjectionMutation::PutIfPresent { .. }]
        ));
        assert_eq!(without_watch.undo.remove_spent_markers.len(), 1);
    }

    #[test]
    fn missing_resolved_prevout_fails_before_commit() {
        let destination = p2wpkh_address(0x02);
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([3; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: destination.script_pubkey(),
            }],
        };
        let mut value = transaction_json(
            &transaction,
            &[Some(PreviousEvidence {
                value: 2_000,
                script: destination.script_pubkey(),
                height: 9,
                coinbase: false,
            })],
        );
        value["vin"][0]
            .as_object_mut()
            .expect("test input must be an object")
            .remove("prevout");
        let block = block(vec![value]);

        let error = BitcoinBlockInterpreter::new(scope(), BitcoinNetwork::Regtest)
            .expect("scope must be valid")
            .inspect(
                &block,
                &[transaction_watch(BitcoinTransactionId::from(
                    transaction.compute_txid(),
                ))],
            )
            .expect_err("missing prevout evidence must fail the block");

        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(error.message.contains("resolved previous output"));
    }
}
