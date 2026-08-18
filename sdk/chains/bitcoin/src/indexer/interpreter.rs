use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::LazyLock,
};

use base::Decimal;
use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexChanges,
    IndexError, IndexErrorKind, IndexScope, IndexUndo, InterpretedBlock, ObservationDraft,
    ObservationDraftStatus, OutputChanges, OutputId, OutputKey, RawBlock, TransactionRef, WatchId,
    WatchSelector, WatchTarget,
};

use crate::{Address, Network, TransactionId};

use super::{
    Block, IndexedOutput, Outpoint, UtxoKey,
    transaction::{Canonicalize, InterpretedTransaction},
};

static CHAIN_ID: LazyLock<ChainId> = LazyLock::new(|| ChainId(crate::CHAIN.to_owned()));
pub(super) static NATIVE_ASSET: LazyLock<AssetId> = LazyLock::new(|| AssetId {
    chain: (*CHAIN_ID).clone(),
    asset: "native".to_owned(),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInterpreter {
    scope: IndexScope,
    network: Network,
}

impl BlockInterpreter {
    pub fn new(scope: IndexScope, network: Network) -> Result<Self, IndexError> {
        if scope.chain != *CHAIN_ID {
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
    pub const fn network(&self) -> Network {
        self.network
    }

    fn validated_watches(
        &self,
        watches: &[WatchTarget<WatchSelector>],
        height: indexing::BlockHeight,
    ) -> Result<ValidatedWatches, IndexError> {
        let mut validated = ValidatedWatches::default();
        for watch in watches {
            validated.add(watch, &self.scope, self.network, height)?;
        }
        Ok(validated)
    }
}

impl IndexBlockInterpreter for BlockInterpreter {
    type Block = Block;
    type Target = WatchSelector;
    type Effect = IndexChanges;
    type Undo = IndexUndo;

    fn inspect(
        &self,
        block: &Self::Block,
        watches: &[WatchTarget<Self::Target>],
    ) -> Result<InterpretedBlock<Self::Effect, Self::Undo>, IndexError> {
        let observed_at = block
            .reference
            .timestamp
            .ok_or_else(|| invalid_block("Bitcoin block timestamp is unavailable"))?;
        let watches = self.validated_watches(watches, block.reference.height)?;

        let mut drafts = Vec::new();
        let mut creates = BTreeMap::<Outpoint, IndexedOutput>::new();
        let mut spends = BTreeMap::<Outpoint, UtxoKey>::new();
        let mut tracked_spends = BTreeMap::<Outpoint, UtxoKey>::new();
        let mut all_spent_outpoints = BTreeSet::new();

        for transaction in block.transactions() {
            let interpreted = InterpretedTransaction::from_transaction(
                transaction,
                block.reference.height,
                self.network,
                &self.scope,
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
                    // but canonical UTXO state and rollback contain no effect.
                    continue;
                }
                if spends.insert(output.outpoint, output).is_some() {
                    return Err(invalid_block(
                        "Bitcoin block spends a watched outpoint more than once",
                    ));
                }
            }
            for output in interpreted.tracked_spends {
                if creates.remove(&output.outpoint).is_some() {
                    continue;
                }
                if tracked_spends.insert(output.outpoint, output).is_some() {
                    return Err(invalid_block(
                        "Bitcoin block contains the same tracked spend more than once",
                    ));
                }
            }
            if !interpreted.watch_ids.is_empty() {
                drafts.push(ObservationDraft {
                    scope: self.scope.clone(),
                    transaction_id: transaction.id.canonical(&self.scope),
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
        let tracked_spends: Vec<_> = tracked_spends.into_values().collect();
        let created = creates
            .iter()
            .map(|output| indexed_output(output, &self.scope))
            .collect::<Result<Vec<_>, _>>()?;
        let spent = spends
            .iter()
            .map(|output| canonical_key(output, &self.scope))
            .collect::<Vec<_>>();
        let tracked_spends = tracked_spends
            .iter()
            .map(|output| canonical_key(output, &self.scope))
            .collect::<Vec<_>>();
        let effect = IndexChanges {
            outputs: OutputChanges {
                created,
                spent: spent.clone(),
                tracked_spends: tracked_spends.clone(),
            },
        };
        Ok(InterpretedBlock {
            block: block.reference.clone(),
            drafts,
            effect,
            undo: IndexUndo {
                created: creates
                    .iter()
                    .map(|output| output_key(output, &self.scope))
                    .collect(),
                spent: spent.into_iter().chain(tracked_spends).collect(),
            },
            raw: RawBlock {
                block: block.raw().to_vec(),
                receipts: Vec::new(),
            },
        })
    }

    fn backfill_effect(&self, mut effect: Self::Effect) -> Result<Self::Effect, IndexError> {
        effect.outputs.tracked_spends.clear();
        Ok(effect)
    }
}

fn output_key(output: &IndexedOutput, scope: &IndexScope) -> OutputKey {
    OutputKey {
        address: output.address.clone().canonical(scope),
        output: OutputId {
            transaction: output.outpoint.transaction_id.canonical(scope),
            index: output.outpoint.output_index,
        },
    }
}

fn canonical_key(output: &UtxoKey, scope: &IndexScope) -> OutputKey {
    OutputKey {
        address: output.address.clone().canonical(scope),
        output: OutputId {
            transaction: output.outpoint.transaction_id.canonical(scope),
            index: output.outpoint.output_index,
        },
    }
}

fn indexed_output(
    output: &IndexedOutput,
    scope: &IndexScope,
) -> Result<indexing::IndexedOutput, IndexError> {
    Ok(indexing::IndexedOutput {
        id: OutputId {
            transaction: output.outpoint.transaction_id.canonical(scope),
            index: output.outpoint.output_index,
        },
        address: output.address.clone().canonical(scope),
        asset: (*NATIVE_ASSET).clone(),
        amount: Decimal::from(output.value.0),
        evidence: output.script_pubkey.clone(),
        created_at: output.created_height,
        coinbase: output.coinbase,
    })
}

#[derive(Default)]
pub(super) struct ValidatedWatches {
    pub(super) active_addresses: BTreeMap<String, BTreeSet<WatchId>>,
    pub(super) transactions: BTreeMap<TransactionId, BTreeSet<WatchId>>,
}

impl ValidatedWatches {
    fn add(
        &mut self,
        watch: &WatchTarget<WatchSelector>,
        scope: &IndexScope,
        network: Network,
        height: indexing::BlockHeight,
    ) -> Result<(), IndexError> {
        if watch.scope != *scope {
            return Err(invalid_watch("Bitcoin watch belongs to a different scope"));
        }
        if watch.target != watch.selector {
            return Err(invalid_watch(
                "Bitcoin watch target does not match its canonical selector",
            ));
        }
        match &watch.selector {
            WatchSelector::Address(selector) => self.add_address(watch, selector, network, height),
            WatchSelector::Transaction(selector) => self.add_transaction(watch, selector, height),
        }
    }

    fn add_address(
        &mut self,
        watch: &WatchTarget<WatchSelector>,
        selector: &CanonicalAddress,
        network: Network,
        height: indexing::BlockHeight,
    ) -> Result<(), IndexError> {
        let canonical = Address::parse_for_network(&selector.value, network)
            .map_err(|_| invalid_watch("Bitcoin watch address is invalid or wrong-network"))?;
        let script = canonical
            .script_pubkey_for_network(network)
            .map_err(|_| invalid_watch("Bitcoin watch address cannot produce a script"))?;
        if !script.is_p2wpkh() && !script.is_p2tr() {
            return Err(invalid_watch(
                "Bitcoin v1 address watches support P2WPKH and P2TR only",
            ));
        }
        if !selector.belongs_to(&watch.scope) || selector.value != canonical.encoded() {
            return Err(invalid_watch(
                "Bitcoin watch target does not match its canonical address selector",
            ));
        }
        if watch.is_active_at(height) {
            self.active_addresses
                .entry(canonical.encoded().to_owned())
                .or_default()
                .insert(watch.id.clone());
        }
        Ok(())
    }

    fn add_transaction(
        &mut self,
        watch: &WatchTarget<WatchSelector>,
        selector: &TransactionRef,
        height: indexing::BlockHeight,
    ) -> Result<(), IndexError> {
        if !selector.belongs_to(&watch.scope) {
            return Err(invalid_watch(
                "Bitcoin watch target does not match its canonical transaction selector",
            ));
        }
        let transaction_id = TransactionId::from_str(&selector.value)
            .map_err(|_| invalid_watch("Bitcoin transaction watch is invalid"))?;
        if watch.is_active_at(height) {
            self.transactions
                .entry(transaction_id)
                .or_default()
                .insert(watch.id.clone());
        }
        Ok(())
    }
}

fn invalid_watch(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidWatch, message, false)
}

pub(super) fn invalid_block(message: impl ToString) -> IndexError {
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
    use indexing::{BlockHash, BlockHeight, MovementId, MovementKind, WatchSelector};
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
            chain: (*CHAIN_ID).clone(),
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

    fn block(transactions: Vec<Value>) -> Block {
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
        Block::parse(
            raw_block,
            Some(BlockHeight(10)),
            Some(&BlockHash(native_hash.to_byte_array().to_vec())),
            Network::Regtest,
        )
        .expect("test block must parse once at its boundary")
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

    fn address_watch(id: &str, address: &Address) -> WatchTarget<WatchSelector> {
        let address = crate::Address::from_encoded(address.to_string());
        let selector = WatchSelector::Address(address.canonical(&scope()));
        WatchTarget {
            id: WatchId(id.to_owned()),
            scope: scope(),
            selector: selector.clone(),
            target: selector,
            idempotency_key: format!("{id}-key"),
            start_height: BlockHeight(0),
            registered_at: None,
            inactive_from: None,
        }
    }

    fn transaction_watch(id: TransactionId) -> WatchTarget<WatchSelector> {
        let selector = WatchSelector::Transaction(id.canonical(&scope()));
        WatchTarget {
            id: WatchId("watch-tx".to_owned()),
            scope: scope(),
            selector: selector.clone(),
            target: selector,
            idempotency_key: "tx-key".to_owned(),
            start_height: BlockHeight(0),
            registered_at: None,
            inactive_from: None,
        }
    }

    #[test]
    fn same_block_spend_nets_utxo_state_while_emitting_movements() {
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

        let interpreted = BlockInterpreter::new(scope(), Network::Regtest)
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
        assert_eq!(spend.movements[0].kind(), MovementKind::Input);
        assert_eq!(spend.movements[1].kind(), MovementKind::Output);
        assert_eq!(
            spend.movements[0].id(),
            &MovementId(format!("{}:vin:0", spending.compute_txid()))
        );
        assert_eq!(
            spend
                .fee
                .as_ref()
                .expect("normal transaction has a fee")
                .amount,
            Decimal::from(1_000_u64)
        );
        assert_eq!(
            spend
                .fee
                .as_ref()
                .expect("normal transaction has a fee")
                .amount
                .scale(),
            0
        );
        assert_eq!(interpreted.effect.outputs.created.len(), 1);
        assert!(interpreted.effect.outputs.spent.is_empty());
        assert!(interpreted.effect.outputs.tracked_spends.is_empty());
        assert_eq!(interpreted.undo.created.len(), 1);
        assert_eq!(interpreted.undo.spent.len(), 0);
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
        let id = TransactionId::from(transaction.compute_txid());

        let interpreted = BlockInterpreter::new(scope(), Network::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[transaction_watch(id)])
            .expect("valid watched transaction must interpret");

        let draft = &interpreted.drafts[0];
        assert_eq!(draft.movements.len(), 3);
        assert_eq!(draft.movements[2].to(), None);
        assert_eq!(draft.fee.as_ref().expect("fee must exist").payer, None);
        assert_eq!(interpreted.effect.outputs.tracked_spends.len(), 2);
        assert!(interpreted.effect.outputs.created.is_empty());
        assert!(interpreted.effect.outputs.spent.is_empty());
        assert_eq!(interpreted.undo.spent.len(), 2);
    }

    #[test]
    fn active_address_spend_is_recorded_directly() {
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

        let interpreted = BlockInterpreter::new(scope(), Network::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[address_watch("active-source", &source)])
            .expect("active watch spend must interpret");

        assert_eq!(interpreted.drafts.len(), 1);
        assert_eq!(interpreted.effect.outputs.spent.len(), 1);
        assert!(interpreted.effect.outputs.created.is_empty());
        assert!(interpreted.effect.outputs.tracked_spends.is_empty());
        assert!(interpreted.undo.created.is_empty());
        assert_eq!(interpreted.undo.spent.len(), 1);
    }

    #[test]
    fn inactive_address_spend_is_limited_to_previously_tracked_outputs() {
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

        let interpreted = BlockInterpreter::new(scope(), Network::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[watch])
            .expect("inactive tracked-output interest must remain valid");

        assert!(interpreted.drafts.is_empty());
        assert_eq!(interpreted.effect.outputs.tracked_spends.len(), 1);
        assert!(interpreted.effect.outputs.created.is_empty());
        assert!(interpreted.effect.outputs.spent.is_empty());
        assert!(interpreted.undo.created.is_empty());
        assert_eq!(interpreted.undo.spent.len(), 1);

        let without_watch = BlockInterpreter::new(scope(), Network::Regtest)
            .expect("scope must be valid")
            .inspect(&block, &[])
            .expect("unwatched supported input must interpret");
        assert!(without_watch.drafts.is_empty());
        assert_eq!(without_watch.effect.outputs.tracked_spends.len(), 1);
        assert_eq!(without_watch.undo.spent.len(), 1);
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
        let native_hash = bitcoin::BlockHash::from_byte_array([0xaa; 32]);
        let parent = bitcoin::BlockHash::from_byte_array([0xbb; 32]);
        let raw = serde_json::to_vec(&json!({
            "hash": native_hash.to_string(),
            "height": 10,
            "previousblockhash": parent.to_string(),
            "time": 100,
            "nTx": 1,
            "tx": [value]
        }))
        .expect("test block JSON must encode");

        let error = Block::parse(
            raw,
            Some(BlockHeight(10)),
            Some(&BlockHash(native_hash.to_byte_array().to_vec())),
            Network::Regtest,
        )
        .expect_err("missing prevout evidence must fail while parsing the block");

        assert_eq!(error.kind, crate::ChainErrorKind::InvalidTransaction);
        assert!(error.message.contains("resolved previous output"));
    }
}
