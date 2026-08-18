use std::{collections::BTreeMap, sync::Arc};

use base::{Address as BaseAddress, TransactionEnvelope, TransactionId as BaseId};
use indexing::IndexScope;
use wallets::{
    Collector, Error as WalletError, ErrorKind, PreparedCollection, SelectedOutput, Wallet,
};

use crate::{
    Address, BuildRequest, FeeRate, Fees, Network, Output, Satoshi, SpendSource,
    TransactionBuilder, TransactionId, Utxos,
};

use super::PREPARED_KIND;

struct Source {
    wallet: Arc<dyn Wallet>,
    address: Address,
    outputs: Vec<SelectedOutput>,
}

pub(super) struct BatchCollector {
    scope: IndexScope,
    network: Network,
    utxos: Arc<dyn Utxos>,
    fees: Arc<dyn Fees>,
    fee_target_blocks: u16,
    max_fee_rate: FeeRate,
    sources: Vec<Source>,
    destination: Option<Address>,
}

impl BatchCollector {
    pub(super) fn new(
        scope: IndexScope,
        network: Network,
        utxos: Arc<dyn Utxos>,
        fees: Arc<dyn Fees>,
        fee_target_blocks: u16,
        max_fee_rate: FeeRate,
    ) -> Self {
        Self {
            scope,
            network,
            utxos,
            fees,
            fee_target_blocks,
            max_fee_rate,
            sources: Vec::new(),
            destination: None,
        }
    }

    fn parse_address(&self, address: &BaseAddress) -> Result<Address, WalletError> {
        let value = std::str::from_utf8(address.as_bytes())
            .map_err(|_| invalid(ErrorKind::InvalidAddress, "source address is not UTF-8"))?;
        Address::parse_for_network(value, self.network)
            .map_err(|error| invalid(ErrorKind::InvalidAddress, error))
    }

    async fn selected_inputs(
        &self,
        source: &Source,
        checkpoint: &mut Option<base::BlockRef>,
    ) -> Result<Vec<(SpendSource, Arc<dyn Wallet>)>, WalletError> {
        let set = self
            .utxos
            .utxos(vec![source.address.clone()])
            .await
            .map_err(|error| invalid(ErrorKind::Transaction, error))?;
        if checkpoint
            .as_ref()
            .is_some_and(|expected| expected != &set.checkpoint)
        {
            return Err(invalid(
                ErrorKind::Transaction,
                "indexed output snapshot changed while collecting sources",
            ));
        }
        checkpoint.get_or_insert(set.checkpoint);
        let indexed = set
            .outputs
            .into_iter()
            .map(|output| ((output.transaction_id, output.output_index), output))
            .collect::<BTreeMap<_, _>>();
        source
            .outputs
            .iter()
            .map(|selected| {
                let id = selected
                    .output
                    .transaction
                    .value
                    .parse::<TransactionId>()
                    .map_err(|error| invalid(ErrorKind::Transaction, error))?;
                let key = (id.0, selected.output.index);
                let output = indexed.get(&key).ok_or_else(|| {
                    invalid(
                        ErrorKind::Transaction,
                        "selected output is not spendable by its source wallet",
                    )
                })?;
                let expected = selected
                    .amount
                    .to_atomic_u64(0)
                    .map_err(|error| invalid(ErrorKind::Transaction, error))?;
                if output.value.0 != expected {
                    return Err(invalid(
                        ErrorKind::Transaction,
                        "selected output amount changed after reservation",
                    ));
                }
                let input = SpendSource::from_exact_selection(
                    self.network,
                    &source.address,
                    id,
                    output.output_index,
                    output.value,
                    output.script_pubkey.clone(),
                )
                .map_err(|error| invalid(ErrorKind::Transaction, error))?;
                Ok((input, source.wallet.clone()))
            })
            .collect()
    }

    fn append_inputs(
        selected: Vec<(SpendSource, Arc<dyn Wallet>)>,
        available: &mut Vec<SpendSource>,
        owners: &mut BTreeMap<([u8; 32], u32), Arc<dyn Wallet>>,
    ) {
        for (input, owner) in selected {
            owners.insert((input.transaction_id, input.output_index), owner);
            available.push(input);
        }
    }
}

impl Collector for BatchCollector {
    fn source(
        &mut self,
        wallet: Arc<dyn Wallet>,
        outputs: Vec<SelectedOutput>,
    ) -> Result<(), WalletError> {
        if outputs.is_empty() {
            return Err(invalid(
                ErrorKind::Transaction,
                "collection source has no selected outputs",
            ));
        }
        let address = self.parse_address(&wallet.address())?;
        if self.sources.iter().any(|source| source.address == address) {
            return Err(invalid(
                ErrorKind::Transaction,
                "collection source address is duplicated",
            ));
        }
        let mut seen = BTreeMap::new();
        for source in &self.sources {
            for selected in &source.outputs {
                seen.insert(
                    (
                        selected.output.transaction.value.as_str(),
                        selected.output.index,
                    ),
                    (),
                );
            }
        }
        for selected in &outputs {
            if !selected.output.transaction.belongs_to(&self.scope) {
                return Err(invalid(
                    ErrorKind::Transaction,
                    "selected output belongs to another chain/network scope",
                ));
            }
            selected.amount.to_atomic_u64(0).map_err(|error| {
                invalid(
                    ErrorKind::Transaction,
                    format!("selected output amount is not an atomic value: {error}"),
                )
            })?;
            let key = (
                selected.output.transaction.value.as_str(),
                selected.output.index,
            );
            if seen.insert(key, ()).is_some() {
                return Err(invalid(
                    ErrorKind::Transaction,
                    "selected output is duplicated",
                ));
            }
        }
        self.sources.push(Source {
            wallet,
            address,
            outputs,
        });
        Ok(())
    }

    fn destination(&mut self, address: BaseAddress) -> Result<(), WalletError> {
        if self.destination.is_some() {
            return Err(invalid(
                ErrorKind::Transaction,
                "collection destination is already configured",
            ));
        }
        self.destination = Some(self.parse_address(&address)?);
        Ok(())
    }

    fn prepare<'a>(&'a mut self) -> wallets::FutureResult<'a, PreparedCollection> {
        Box::pin(async move {
            let destination = self.destination.clone().ok_or_else(|| {
                invalid(
                    ErrorKind::Transaction,
                    "collection destination is not configured",
                )
            })?;
            if self.sources.is_empty() {
                return Err(invalid(ErrorKind::Transaction, "collection has no sources"));
            }

            let mut checkpoint = None;
            let mut available = Vec::new();
            let mut owners = BTreeMap::new();
            for source in &self.sources {
                let selected = self.selected_inputs(source, &mut checkpoint).await?;
                Self::append_inputs(selected, &mut available, &mut owners);
            }

            available.sort_by(|left, right| {
                TransactionId(left.transaction_id)
                    .to_string()
                    .cmp(&TransactionId(right.transaction_id).to_string())
                    .then_with(|| left.output_index.cmp(&right.output_index))
            });
            let signers = available
                .iter()
                .map(|input| {
                    owners
                        .get(&(input.transaction_id, input.output_index))
                        .map(Arc::as_ref)
                        .ok_or_else(|| {
                            invalid(ErrorKind::Transaction, "selected output has no owner")
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let fee_rate = self
                .fees
                .estimate(self.fee_target_blocks)
                .await
                .map_err(|error| invalid(ErrorKind::Transaction, error))?;
            if fee_rate > self.max_fee_rate {
                return Err(invalid(
                    ErrorKind::Transaction,
                    "estimated fee rate exceeds the configured maximum",
                ));
            }
            let total_input = available.iter().try_fold(0_u64, |sum, input| {
                sum.checked_add(input.value.0).ok_or_else(|| {
                    invalid(ErrorKind::Transaction, "collection input value overflowed")
                })
            })?;
            let builder = TransactionBuilder::new(
                self.network,
                BuildRequest {
                    available,
                    recipients: vec![Output::from_atomic(destination, Satoshi(0))],
                    change_address: self.sources[0].address.clone(),
                    fee_rate,
                    drain_wallet: true,
                },
            );
            let signed = builder
                .sign_each(&signers)
                .await
                .map_err(|error| invalid(ErrorKind::Transaction, error))?;
            let total_output = signed
                .inspect()
                .map_err(|error| invalid(ErrorKind::Transaction, error))?
                .outputs
                .iter()
                .try_fold(0_u64, |sum, output| {
                    sum.checked_add(output.value.0).ok_or_else(|| {
                        invalid(ErrorKind::Transaction, "collection output value overflowed")
                    })
                })?;
            let fee = total_input.checked_sub(total_output).ok_or_else(|| {
                invalid(
                    ErrorKind::Transaction,
                    "collection outputs exceed selected inputs",
                )
            })?;
            Ok(PreparedCollection {
                transaction: base::SignedTransaction::new(
                    PREPARED_KIND,
                    BaseId::new(signed.id().to_string()),
                    TransactionEnvelope::new(signed.consensus_bytes().to_vec()),
                ),
                fee: wallets::PreparedFee::Exact(base::Decimal::from(fee)),
            })
        })
    }
}

fn invalid(kind: ErrorKind, message: impl std::fmt::Display) -> WalletError {
    WalletError::new(kind, message.to_string())
}
