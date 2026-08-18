use std::{collections::BTreeSet, sync::LazyLock};

use alloy_primitives::U256;
use base::Decimal;
use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexChanges,
    IndexError, IndexErrorKind, IndexScope, IndexUndo, InterpretedBlock, MovementId, NetworkFee,
    ObservationDraft, ObservationDraftStatus, TransactionRef, ValueMovement, WatchId,
    WatchSelector, WatchTarget,
};
use num_bigint::BigUint;

use super::{
    Block,
    model::{ParsedBlock, ParsedLog, ParsedReceipt, ParsedTransaction, encode_hex},
};

const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];
const ZERO_ADDRESS: [u8; 20] = [0; 20];
static CHAIN_ID: LazyLock<ChainId> = LazyLock::new(|| ChainId(crate::CHAIN.to_owned()));
static NATIVE_ASSET: LazyLock<AssetId> = LazyLock::new(|| AssetId {
    chain: (*CHAIN_ID).clone(),
    asset: "native".to_owned(),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInterpreter {
    scope: IndexScope,
}

impl BlockInterpreter {
    pub fn new(scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain != *CHAIN_ID {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum interpreter scope must use the ethereum chain ID",
                false,
            ));
        }
        if scope.network.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum interpreter network slug must not be empty",
                false,
            ));
        }
        Ok(Self { scope })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    fn validate_watch(&self, watch: &WatchTarget<WatchSelector>) -> Result<(), IndexError> {
        if watch.scope != self.scope {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum watch belongs to a different scope",
                false,
            ));
        }
        if watch.target != watch.selector {
            return Err(IndexError::new(
                IndexErrorKind::InvalidWatch,
                "Ethereum watch target does not match its canonical selector",
                false,
            ));
        }
        Ok(())
    }

    fn watch_ids(
        &self,
        transaction: &ParsedTransaction,
        receipt: &ParsedReceipt,
        movements: &[ValueMovement],
        watches: &[WatchTarget<WatchSelector>],
        height: indexing::BlockHeight,
    ) -> Result<Vec<WatchId>, IndexError> {
        let mut touched_addresses = BTreeSet::from([transaction.from]);
        if let Some(to) = transaction.to {
            touched_addresses.insert(to);
        }
        if let Some(contract) = receipt.contract_address {
            touched_addresses.insert(contract);
        }
        for movement in movements {
            for endpoint in [movement.from(), movement.to()].into_iter().flatten() {
                let bytes = parse_canonical_address(endpoint)?;
                touched_addresses.insert(bytes);
            }
        }

        let mut ids = BTreeSet::new();
        for watch in watches {
            self.validate_watch(watch)?;
            if !watch.is_active_at(height) {
                continue;
            }
            if !watch.selector.belongs_to(&self.scope) {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "Ethereum address watch belongs to a different scope",
                    false,
                ));
            }
            let matched = touched_addresses.contains(&parse_canonical_address(&watch.selector)?);
            if matched {
                ids.insert(watch.id.clone());
            }
        }
        Ok(ids.into_iter().collect())
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
        let parsed = ParsedBlock::parse(&block.raw_block, Some(block.reference.height), true)
            .map_err(invalid_block)?;
        if parsed.reference != block.reference {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "retained Ethereum block reference does not match its raw payload",
                false,
            ));
        }
        let receipts =
            ParsedReceipt::parse_all(&block.raw_receipts, &parsed).map_err(invalid_block)?;
        let observed_at = block.reference.timestamp.ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "Ethereum block timestamp is unavailable",
                false,
            )
        })?;

        let mut drafts = Vec::new();
        for (transaction, receipt) in parsed.transactions.iter().zip(&receipts) {
            let fee_amount = receipt
                .effective_gas_price
                .checked_mul(U256::from(receipt.gas_used))
                .ok_or_else(|| {
                    IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "Ethereum receipt fee exceeds 256-bit atomic units",
                        false,
                    )
                })?;
            let fee = NetworkFee {
                asset: (*NATIVE_ASSET).clone(),
                amount: atomic_decimal(fee_amount),
                payer: Some(transaction.from.canonical(&self.scope)),
            };
            let movements = if receipt.succeeded {
                successful_movements(transaction, receipt, &self.scope)?
            } else {
                Vec::new()
            };
            let watch_ids = self.watch_ids(
                transaction,
                receipt,
                &movements,
                watches,
                block.reference.height,
            )?;
            if watch_ids.is_empty() {
                continue;
            }

            let transaction_id = transaction.hash.canonical(&self.scope);
            drafts.push(ObservationDraft {
                scope: self.scope.clone(),
                transaction_id,
                status: if receipt.succeeded {
                    ObservationDraftStatus::Included
                } else {
                    ObservationDraftStatus::Failed {
                        reason: Some("ethereum receipt status is zero".to_owned()),
                    }
                },
                movements,
                fee: Some(fee),
                watch_ids,
                first_seen_at: observed_at,
                observed_at,
            });
        }

        Ok(InterpretedBlock {
            block: block.reference.clone(),
            drafts,
            effect: IndexChanges::default(),
            undo: IndexUndo::default(),
        })
    }
}

fn successful_movements(
    transaction: &ParsedTransaction,
    receipt: &ParsedReceipt,
    scope: &IndexScope,
) -> Result<Vec<ValueMovement>, IndexError> {
    let transaction_id = encode_hex(&transaction.hash);
    let mut movements = Vec::new();
    if !transaction.value.is_zero() {
        let to = transaction.to.or(receipt.contract_address).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "successful value-bearing Ethereum transaction has no recipient",
                false,
            )
        })?;
        movements.push(ValueMovement::Transfer {
            id: MovementId(format!("{transaction_id}:value")),
            asset: (*NATIVE_ASSET).clone(),
            amount: atomic_decimal(transaction.value),
            from: transaction.from.canonical(scope),
            to: to.canonical(scope),
        });
    }

    for log in &receipt.logs {
        if let Some(movement) = transfer_movement(&transaction_id, log, scope) {
            movements.push(movement);
        }
    }
    Ok(movements)
}

fn transfer_movement(
    transaction_id: &str,
    log: &ParsedLog,
    scope: &IndexScope,
) -> Option<ValueMovement> {
    if log.topics.len() != 3 || log.topics[0] != TRANSFER_TOPIC || log.data.len() != 32 {
        return None;
    }
    let from = topic_address(log.topics[1])?;
    let to = topic_address(log.topics[2])?;
    let amount = U256::from_be_slice(&log.data);
    let id = MovementId(format!("{transaction_id}:{}", log.log_index));
    let asset = AssetId {
        chain: (*CHAIN_ID).clone(),
        asset: encode_hex(&log.address),
    };
    if from == ZERO_ADDRESS {
        (to != ZERO_ADDRESS).then(|| ValueMovement::Mint {
            id,
            asset,
            amount: atomic_decimal(amount),
            to: to.canonical(scope),
        })
    } else if to == ZERO_ADDRESS {
        Some(ValueMovement::Burn {
            id,
            asset,
            amount: atomic_decimal(amount),
            from: from.canonical(scope),
        })
    } else {
        Some(ValueMovement::Transfer {
            id,
            asset,
            amount: atomic_decimal(amount),
            from: from.canonical(scope),
            to: to.canonical(scope),
        })
    }
}

fn topic_address(topic: [u8; 32]) -> Option<[u8; 20]> {
    if topic[..12] != [0; 12] {
        return None;
    }
    topic[12..].try_into().ok()
}

fn parse_canonical_address(address: &CanonicalAddress) -> Result<[u8; 20], IndexError> {
    if address.scope.chain != *CHAIN_ID {
        return Err(IndexError::new(
            IndexErrorKind::InvalidBlock,
            "Ethereum movement contains a foreign-chain address",
            false,
        ));
    }
    address
        .value
        .parse::<alloy_primitives::Address>()
        .map(alloy_primitives::Address::into_array)
        .map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "Ethereum movement contains a malformed canonical address",
                false,
            )
        })
}

fn atomic_decimal(value: U256) -> Decimal {
    Decimal::from_atomic(BigUint::from_bytes_be(&value.to_be_bytes::<32>()), 0)
}

trait Canonicalize {
    type Output;

    fn canonical(self, scope: &IndexScope) -> Self::Output;
}

impl Canonicalize for [u8; 20] {
    type Output = CanonicalAddress;

    fn canonical(self, scope: &IndexScope) -> Self::Output {
        CanonicalAddress {
            scope: scope.clone(),
            value: encode_hex(&self),
        }
    }
}

impl Canonicalize for [u8; 32] {
    type Output = TransactionRef;

    fn canonical(self, scope: &IndexScope) -> Self::Output {
        TransactionRef {
            scope: scope.clone(),
            value: encode_hex(&self),
        }
    }
}

fn invalid_block(error: impl ToString) -> IndexError {
    IndexError::new(IndexErrorKind::InvalidBlock, error.to_string(), false)
}

#[cfg(test)]
#[path = "interpreter_test.rs"]
mod tests;
