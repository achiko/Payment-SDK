use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

use base::Decimal;
use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexChanges,
    IndexError, IndexErrorKind, IndexScope, IndexUndo, InterpretedBlock, ObservationDraft,
    ObservationDraftStatus, OutputChanges, OutputId, OutputKey, WatchId, WatchSelector,
    WatchTarget,
};

use crate::{Address, Network};

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
        })
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
        self.add_address(watch, &watch.selector, network, height)
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
                "Bitcoin address watches support P2WPKH and P2TR only",
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
}

fn invalid_watch(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidWatch, message, false)
}

pub(super) fn invalid_block(message: impl ToString) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, message.to_string(), false)
}

#[cfg(test)]
#[path = "interpreter_test.rs"]
mod tests;
