use std::{collections::BTreeSet, sync::LazyLock};

use alloy_primitives::U256;
use base::Decimal;
use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexError,
    IndexErrorKind, IndexScope, InterpretedBlock, MovementId, NetworkFee, ObservationDraft,
    ObservationDraftStatus, OutputChanges, TransactionRef, ValueMovement,
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

struct Movements(Vec<ValueMovement>);

impl Movements {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn successful(
        transaction: &ParsedTransaction,
        receipt: &ParsedReceipt,
        scope: &IndexScope,
    ) -> Result<Self, IndexError> {
        let transaction_id = encode_hex(&transaction.hash);
        let mut movements = Self::new();
        if !transaction.value.is_zero() {
            let to = transaction.to.or(receipt.contract_address).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "successful value-bearing Ethereum transaction has no recipient",
                    false,
                )
            })?;
            movements.0.push(ValueMovement::Transfer {
                id: MovementId(format!("{transaction_id}:value")),
                asset: (*NATIVE_ASSET).clone(),
                amount: atomic_decimal(transaction.value),
                from: transaction.from.canonical(scope),
                to: to.canonical(scope),
            });
        }

        movements.0.extend(
            receipt
                .logs
                .iter()
                .filter_map(|log| log.movement(&transaction_id, scope)),
        );
        Ok(movements)
    }

    fn into_vec(self) -> Vec<ValueMovement> {
        self.0
    }
}

impl ParsedLog {
    fn movement(&self, transaction_id: &str, scope: &IndexScope) -> Option<ValueMovement> {
        let [signature, from, to] = self.topics.as_slice() else {
            return None;
        };
        if *signature != TRANSFER_TOPIC
            || self.data.len() != 32
            || from[..12] != [0; 12]
            || to[..12] != [0; 12]
        {
            return None;
        }
        let from: [u8; 20] = from[12..].try_into().ok()?;
        let to: [u8; 20] = to[12..].try_into().ok()?;
        let amount = U256::from_be_slice(&self.data);
        let id = MovementId(format!("{transaction_id}:{}", self.log_index));
        let asset = AssetId {
            chain: (*CHAIN_ID).clone(),
            asset: encode_hex(&self.address),
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
}

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

    fn matches_address(
        &self,
        transaction: &ParsedTransaction,
        receipt: &ParsedReceipt,
        movements: &Movements,
        addresses: &[CanonicalAddress],
    ) -> Result<bool, IndexError> {
        let mut touched_addresses = BTreeSet::from([transaction.from]);
        if let Some(to) = transaction.to {
            touched_addresses.insert(to);
        }
        if let Some(contract) = receipt.contract_address {
            touched_addresses.insert(contract);
        }
        for movement in &movements.0 {
            for endpoint in [movement.from(), movement.to()].into_iter().flatten() {
                let bytes = parse_canonical_address(endpoint)?;
                touched_addresses.insert(bytes);
            }
        }

        for address in addresses {
            if !address.belongs_to(&self.scope) {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "Ethereum address filter belongs to a different scope",
                    false,
                ));
            }
            if touched_addresses.contains(&parse_canonical_address(address)?) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl IndexBlockInterpreter for BlockInterpreter {
    type Block = Block;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError> {
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
        let mut transactions = Vec::new();
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
                Movements::successful(transaction, receipt, &self.scope)?
            } else {
                Movements::new()
            };
            if !self.matches_address(transaction, receipt, &movements, addresses)? {
                continue;
            }

            let transaction_id = transaction.hash.canonical(&self.scope);
            transactions.push(ObservationDraft {
                scope: self.scope.clone(),
                transaction_id,
                status: if receipt.succeeded {
                    ObservationDraftStatus::Included
                } else {
                    ObservationDraftStatus::Failed {
                        reason: Some("ethereum receipt status is zero".to_owned()),
                    }
                },
                movements: movements.into_vec(),
                fee: Some(fee),
            });
        }

        Ok(InterpretedBlock {
            block: block.reference.clone(),
            transactions,
            outputs: OutputChanges::default(),
        })
    }
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
