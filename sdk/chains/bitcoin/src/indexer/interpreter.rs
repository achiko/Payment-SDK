use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexError,
    IndexErrorKind, IndexScope, InterpretedBlock, ObservationDraft, ObservationDraftStatus,
    OutputChanges,
};

use crate::{Address, Network};

use super::{Block, IndexedOutput, Outpoint, UtxoKey, transaction::Canonicalize};

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
}

impl IndexBlockInterpreter for BlockInterpreter {
    type Block = Block;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError> {
        let addresses = ValidatedAddresses::new(addresses, &self.scope, self.network)?;

        let mut transactions = Vec::new();
        let mut creates = BTreeMap::<Outpoint, IndexedOutput>::new();
        let mut spends = BTreeMap::<Outpoint, UtxoKey>::new();
        let mut tracked_spends = BTreeMap::<Outpoint, UtxoKey>::new();
        let mut all_spent_outpoints = BTreeSet::new();

        for transaction in block.transactions() {
            let interpreted = transaction.interpret(
                block.reference.height,
                self.network,
                &self.scope,
                &addresses,
                &mut all_spent_outpoints,
            )?;
            for output in interpreted.creates {
                if creates.insert(output.outpoint, output).is_some() {
                    return Err(invalid_block(
                        "Bitcoin block creates a duplicate indexed outpoint",
                    ));
                }
            }
            for output in interpreted.spends {
                if creates.remove(&output.outpoint).is_some() {
                    // An indexed output created and spent within this block did
                    // not exist before or after the block. Movements remain,
                    // but canonical UTXO state contains no change.
                    continue;
                }
                if spends.insert(output.outpoint, output).is_some() {
                    return Err(invalid_block(
                        "Bitcoin block spends an indexed outpoint more than once",
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
            if interpreted.relevant {
                transactions.push(ObservationDraft {
                    scope: self.scope.clone(),
                    transaction_id: transaction.id.canonical(&self.scope),
                    status: ObservationDraftStatus::Included,
                    movements: interpreted.movements,
                    fee: interpreted.fee,
                });
            }
        }

        let creates: Vec<_> = creates.into_values().collect();
        let spends: Vec<_> = spends.into_values().collect();
        let tracked_spends: Vec<_> = tracked_spends.into_values().collect();
        let created = creates
            .iter()
            .map(|output| output.canonical(&self.scope))
            .collect();
        let spent = spends
            .iter()
            .map(|output| output.canonical(&self.scope))
            .collect();
        let tracked_spends = tracked_spends
            .iter()
            .map(|output| output.canonical(&self.scope))
            .collect();
        let outputs = OutputChanges {
            created,
            spent,
            tracked_spends,
        };
        Ok(InterpretedBlock {
            block: block.reference.clone(),
            transactions,
            outputs,
        })
    }
}

#[derive(Default)]
pub(super) struct ValidatedAddresses {
    addresses: BTreeSet<String>,
}

impl ValidatedAddresses {
    fn new(
        addresses: &[CanonicalAddress],
        scope: &IndexScope,
        network: Network,
    ) -> Result<Self, IndexError> {
        let mut validated = Self::default();
        for address in addresses {
            validated.add(address, scope, network)?;
        }
        Ok(validated)
    }

    pub(super) fn contains(&self, address: &Address) -> bool {
        self.addresses.contains(address.encoded())
    }

    fn add(
        &mut self,
        address: &CanonicalAddress,
        scope: &IndexScope,
        network: Network,
    ) -> Result<(), IndexError> {
        if !address.belongs_to(scope) {
            return Err(invalid_address(
                "Bitcoin indexed address belongs to a different scope",
            ));
        }
        let canonical = Address::parse_for_network(&address.value, network)
            .map_err(|_| invalid_address("Bitcoin indexed address is invalid or wrong-network"))?;
        let script = canonical
            .script_pubkey_for_network(network)
            .map_err(|_| invalid_address("Bitcoin indexed address cannot produce a script"))?;
        if !script.is_p2wpkh() && !script.is_p2tr() {
            return Err(invalid_address(
                "Bitcoin indexing supports P2WPKH and P2TR addresses only",
            ));
        }
        if address.value != canonical.encoded() {
            return Err(invalid_address("Bitcoin indexed address is not canonical"));
        }
        self.addresses.insert(canonical.encoded().to_owned());
        Ok(())
    }
}

fn invalid_address(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidRequest, message, false)
}

pub(super) fn invalid_block(message: impl ToString) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, message.to_string(), false)
}

#[cfg(test)]
#[path = "interpreter_test.rs"]
mod tests;
