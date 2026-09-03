use std::collections::BTreeSet;

use indexing::{AssetId, IndexError, IndexScope, MovementId, ValueMovement};
use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

use crate::Address;

use super::{canonical, invalid_block, wire::Transaction};

#[derive(Debug, Default)]
pub(super) struct Movements(Vec<Movement>);

#[derive(Debug)]
struct Movement {
    id: String,
    source: Address,
    destination: Address,
    lamports: u64,
}

impl Movements {
    pub fn decode(transaction: &Transaction) -> Result<Self, IndexError> {
        let mut values = Vec::new();
        for (outer_index, instruction) in transaction.instructions().iter().enumerate() {
            if let Some(movement) = Movement::decode(
                transaction,
                instruction,
                format!("{}:ix:{outer_index}", transaction.signature()),
            )? {
                values.push(movement);
            }
            if let Some(inner) = transaction
                .inner()
                .and_then(|groups| groups.get(&outer_index))
            {
                for (inner_ordinal, instruction) in inner.iter().enumerate() {
                    if let Some(movement) = Movement::decode(
                        transaction,
                        instruction,
                        format!(
                            "{}:ix:{outer_index}:inner:{inner_ordinal}",
                            transaction.signature()
                        ),
                    )? {
                        values.push(movement);
                    }
                }
            }
        }
        Ok(Self(values))
    }

    pub fn touches(&self, address: &Address) -> bool {
        self.0
            .iter()
            .any(|movement| movement.source == *address || movement.destination == *address)
    }

    pub fn touches_any(&self, addresses: &BTreeSet<Address>) -> bool {
        self.0.iter().any(|movement| {
            addresses.contains(&movement.source) || addresses.contains(&movement.destination)
        })
    }

    pub fn delta(&self, address: &Address) -> i128 {
        self.0.iter().fold(0_i128, |delta, movement| {
            let amount = i128::from(movement.lamports);
            let outgoing = if movement.source == *address {
                amount
            } else {
                0
            };
            let incoming = if movement.destination == *address {
                amount
            } else {
                0
            };
            delta + incoming - outgoing
        })
    }

    pub fn into_values(self, scope: &IndexScope, asset: &AssetId) -> Vec<ValueMovement> {
        self.0
            .into_iter()
            .map(|movement| ValueMovement::Transfer {
                id: MovementId(movement.id),
                asset: asset.clone(),
                amount: base::Decimal::from_atomic(movement.lamports.into(), 0),
                from: canonical(&movement.source, scope),
                to: canonical(&movement.destination, scope),
            })
            .collect()
    }
}

impl Movement {
    fn decode(
        transaction: &Transaction,
        instruction: &super::wire::Instruction,
        id: String,
    ) -> Result<Option<Self>, IndexError> {
        if transaction.key(instruction.program).as_bytes() != SYSTEM_ID.as_array() {
            return Ok(None);
        }
        let data = bs58::decode(&instruction.data)
            .into_vec()
            .map_err(|_| invalid_block("Solana System instruction data is not canonical Base58"))?;
        if bs58::encode(&data).into_string() != instruction.data {
            return Err(invalid_block(
                "Solana System instruction data is not canonical Base58",
            ));
        }
        let system = bincode::deserialize::<SystemInstruction>(&data)
            .map_err(|_| invalid_block("Solana System instruction data is malformed"))?;
        if bincode::serialize(&system).ok().as_deref() != Some(data.as_slice()) {
            return Err(invalid_block(
                "Solana System instruction data has a non-canonical encoding",
            ));
        }
        let (source, destination, lamports) = match system {
            SystemInstruction::Transfer { lamports } => {
                let [source, destination, ..] = instruction.accounts.as_slice() else {
                    return Err(invalid_block(
                        "Solana System transfer is missing an account",
                    ));
                };
                (*source, *destination, lamports)
            }
            SystemInstruction::TransferWithSeed { lamports, .. } => {
                let [source, _, destination, ..] = instruction.accounts.as_slice() else {
                    return Err(invalid_block(
                        "Solana System transfer-with-seed is missing an account",
                    ));
                };
                (*source, *destination, lamports)
            }
            _ => return Ok(None),
        };
        Ok((lamports != 0).then(|| Self {
            id,
            source: transaction.key(source).clone(),
            destination: transaction.key(destination).clone(),
            lamports,
        }))
    }
}
