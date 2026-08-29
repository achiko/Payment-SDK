use std::{collections::BTreeMap, str::FromStr};

use indexing::IndexError;
use serde::Deserialize;
use serde_json::Value;
use solana_signature::Signature;

use crate::Address;

use super::{invalid_block, movement::Movements};

#[derive(Debug)]
pub(super) struct Transactions(Vec<Transaction>);

#[derive(Debug)]
pub(super) struct Transaction {
    signature: String,
    keys: Vec<AccountKey>,
    instructions: Vec<Instruction>,
    inner: Option<BTreeMap<usize, Vec<Instruction>>>,
    pre_balances: Vec<u64>,
    post_balances: Vec<u64>,
    fee: u64,
    succeeded: bool,
}

#[derive(Debug)]
struct AccountKey {
    address: Address,
    writable: bool,
}

#[derive(Debug)]
pub(super) struct Instruction {
    pub program: usize,
    pub accounts: Vec<usize>,
    pub data: String,
}

impl Transactions {
    pub fn parse(raw: &[u8]) -> Result<Self, IndexError> {
        let block: BlockWire = serde_json::from_slice(raw)
            .map_err(|_| invalid_block("Solana block transactions have an invalid RPC shape"))?;
        let mut signatures = std::collections::BTreeSet::new();
        let mut transactions = Vec::with_capacity(block.transactions.len());
        for wire in block.transactions {
            let transaction = Transaction::parse(wire)?;
            if !signatures.insert(transaction.signature.clone()) {
                return Err(invalid_block(
                    "Solana block contains duplicate first signatures",
                ));
            }
            transactions.push(transaction);
        }
        Ok(Self(transactions))
    }

    pub fn values(&self) -> &[Transaction] {
        &self.0
    }
}

impl Transaction {
    fn parse(wire: TransactionWire) -> Result<Self, IndexError> {
        let version = Version::parse(&wire.version)?;
        let message = wire.transaction.message;
        let required = usize::from(message.header.num_required_signatures);
        let readonly_signed = usize::from(message.header.num_readonly_signed_accounts);
        let readonly_unsigned = usize::from(message.header.num_readonly_unsigned_accounts);
        let static_count = message.account_keys.len();
        if required == 0
            || required > static_count
            || readonly_signed >= required
            || readonly_unsigned > static_count - required
            || wire.transaction.signatures.len() != required
        {
            return Err(invalid_block(
                "Solana transaction has incoherent signer or message-header cardinality",
            ));
        }

        let mut canonical_signatures = Vec::with_capacity(required);
        for text in wire.transaction.signatures {
            let signature = Signature::from_str(&text)
                .map_err(|_| invalid_block("Solana transaction contains a malformed signature"))?;
            if signature.to_string() != text {
                return Err(invalid_block(
                    "Solana transaction contains a non-canonical signature",
                ));
            }
            canonical_signatures.push(text);
        }

        let mut keys = Vec::with_capacity(static_count);
        for (index, text) in message.account_keys.into_iter().enumerate() {
            let writable = if index < required {
                index < required - readonly_signed
            } else {
                index < static_count - readonly_unsigned
            };
            keys.push(AccountKey {
                address: parse_address(&text)?,
                writable,
            });
        }

        let meta = wire
            .meta
            .ok_or_else(|| invalid_block("Solana transaction metadata is missing"))?;
        match (version, meta.loaded_addresses) {
            (Version::Legacy, Some(loaded))
                if !loaded.writable.is_empty() || !loaded.readonly.is_empty() =>
            {
                return Err(invalid_block(
                    "legacy Solana transaction contains loaded addresses",
                ));
            }
            (Version::Legacy, _) => {}
            (Version::Zero, Some(loaded)) => {
                for text in loaded.writable {
                    keys.push(AccountKey {
                        address: parse_address(&text)?,
                        writable: true,
                    });
                }
                for text in loaded.readonly {
                    keys.push(AccountKey {
                        address: parse_address(&text)?,
                        writable: false,
                    });
                }
            }
            (Version::Zero, None) => {
                return Err(invalid_block(
                    "version-zero Solana transaction is missing loaded addresses",
                ));
            }
        }
        if keys.len() > usize::from(u8::MAX) + 1 {
            return Err(invalid_block(
                "Solana transaction resolves more than 256 account keys",
            ));
        }
        let mut unique = std::collections::BTreeSet::new();
        if keys.iter().any(|key| !unique.insert(key.address.clone())) {
            return Err(invalid_block(
                "Solana transaction resolves duplicate account keys",
            ));
        }
        if meta.pre_balances.len() != keys.len() || meta.post_balances.len() != keys.len() {
            return Err(invalid_block(
                "Solana transaction balances do not match resolved account keys",
            ));
        }

        let instructions = parse_instructions(message.instructions, keys.len())?;
        let inner = meta
            .inner_instructions
            .map(|groups| parse_inner(groups, instructions.len(), keys.len()))
            .transpose()?;
        keys.first()
            .filter(|payer| payer.writable)
            .ok_or_else(|| invalid_block("Solana transaction has no writable fee payer"))?;

        Ok(Self {
            signature: canonical_signatures.remove(0),
            keys,
            instructions,
            inner,
            pre_balances: meta.pre_balances,
            post_balances: meta.post_balances,
            fee: meta.fee,
            succeeded: meta.err.is_null(),
        })
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    pub fn fee_payer(&self) -> &Address {
        &self.keys[0].address
    }

    pub const fn fee(&self) -> u64 {
        self.fee
    }

    pub const fn succeeded(&self) -> bool {
        self.succeeded
    }

    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }

    pub fn inner(&self) -> Option<&BTreeMap<usize, Vec<Instruction>>> {
        self.inner.as_ref()
    }

    pub fn key(&self, index: usize) -> &Address {
        &self.keys[index].address
    }

    pub fn selected_effects<'a>(
        &'a self,
        selected: &'a std::collections::BTreeSet<Address>,
        movements: &Movements,
    ) -> Vec<&'a Address> {
        self.keys
            .iter()
            .filter(|key| {
                selected.contains(&key.address)
                    && (key.writable
                        || key.address == *self.fee_payer()
                        || movements.touches(&key.address))
            })
            .map(|key| &key.address)
            .collect()
    }

    pub fn reconcile(&self, address: &Address, movements: &Movements) -> Result<(), IndexError> {
        let index = self
            .keys
            .iter()
            .position(|key| &key.address == address)
            .ok_or_else(|| {
                invalid_block("selected Solana account is absent from balance vectors")
            })?;
        let actual = i128::from(self.post_balances[index]) - i128::from(self.pre_balances[index]);
        let mut expected = movements.delta(address);
        if address == self.fee_payer() {
            expected -= i128::from(self.fee);
        }
        if actual != expected {
            return Err(invalid_block(
                "selected Solana account has an unsupported native SOL value effect",
            ));
        }
        Ok(())
    }
}

fn parse_instructions(
    wires: Vec<InstructionWire>,
    key_count: usize,
) -> Result<Vec<Instruction>, IndexError> {
    wires
        .into_iter()
        .map(|wire| {
            let program = usize::from(wire.program_id_index);
            let accounts = wire
                .accounts
                .into_iter()
                .map(usize::from)
                .collect::<Vec<_>>();
            if program >= key_count || accounts.iter().any(|index| *index >= key_count) {
                return Err(invalid_block(
                    "Solana compiled instruction contains an invalid account index",
                ));
            }
            Ok(Instruction {
                program,
                accounts,
                data: wire.data,
            })
        })
        .collect()
}

fn parse_inner(
    groups: Vec<InnerWire>,
    outer_count: usize,
    key_count: usize,
) -> Result<BTreeMap<usize, Vec<Instruction>>, IndexError> {
    let mut inner = BTreeMap::new();
    for group in groups {
        let index = usize::from(group.index);
        if index >= outer_count
            || inner
                .insert(index, parse_instructions(group.instructions, key_count)?)
                .is_some()
        {
            return Err(invalid_block(
                "Solana transaction contains an invalid or duplicate inner-instruction group",
            ));
        }
    }
    Ok(inner)
}

fn parse_address(text: &str) -> Result<Address, IndexError> {
    text.parse::<Address>()
        .map_err(|_| invalid_block("Solana transaction contains a malformed canonical address"))
}

#[derive(Clone, Copy)]
enum Version {
    Legacy,
    Zero,
}

impl Version {
    fn parse(value: &Value) -> Result<Self, IndexError> {
        match value {
            Value::String(text) if text == "legacy" => Ok(Self::Legacy),
            Value::Number(number) if number.as_u64() == Some(0) => Ok(Self::Zero),
            _ => Err(invalid_block(
                "Solana block contains an unsupported transaction version",
            )),
        }
    }
}

#[derive(Deserialize)]
struct BlockWire {
    transactions: Vec<TransactionWire>,
}

#[derive(Deserialize)]
struct TransactionWire {
    transaction: SignedWire,
    meta: Option<MetaWire>,
    version: Value,
}

#[derive(Deserialize)]
struct SignedWire {
    signatures: Vec<String>,
    message: MessageWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageWire {
    header: HeaderWire,
    account_keys: Vec<String>,
    instructions: Vec<InstructionWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeaderWire {
    num_required_signatures: u8,
    num_readonly_signed_accounts: u8,
    num_readonly_unsigned_accounts: u8,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstructionWire {
    program_id_index: u8,
    accounts: Vec<u8>,
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaWire {
    err: Value,
    fee: u64,
    pre_balances: Vec<u64>,
    post_balances: Vec<u64>,
    inner_instructions: Option<Vec<InnerWire>>,
    loaded_addresses: Option<LoadedWire>,
}

#[derive(Deserialize)]
struct InnerWire {
    index: u8,
    instructions: Vec<InstructionWire>,
}

#[derive(Deserialize)]
struct LoadedWire {
    writable: Vec<String>,
    readonly: Vec<String>,
}
