use std::sync::Arc;

use base::{
    SignedTransaction as Prepared, TransactionEnvelope as Envelope, TransactionId as BaseId,
};
use wallets::{Error, ErrorKind, SendError, SendFuture, Sender, Transfer, Wallet};

use crate::transaction::{BatchBuilder, Funding};
use crate::wallet::PREPARED_KIND;
use crate::{Address, FeeRate, Fees, IndexUtxos, Network, Output, SpendSource, TransactionId};

struct Source {
    wallet: Arc<dyn Wallet>,
    address: Address,
    recipients: Vec<Output>,
}

pub(crate) struct Batch {
    network: Network,
    utxos: Arc<IndexUtxos>,
    fees: Arc<dyn Fees>,
    fee_target_blocks: u16,
    max_fee_rate: FeeRate,
}

impl Batch {
    pub(crate) fn new(
        network: Network,
        utxos: Arc<IndexUtxos>,
        fees: Arc<dyn Fees>,
        fee_target_blocks: u16,
        max_fee_rate: FeeRate,
    ) -> Self {
        Self {
            network,
            utxos,
            fees,
            fee_target_blocks,
            max_fee_rate,
        }
    }

    fn parse(&self, transfer: Transfer) -> Result<(Arc<dyn Wallet>, Address, Output), Error> {
        let source = native_address(&transfer.wallet.address(), self.network)?;
        let destination = transfer.wallet.parse_address(&transfer.to)?;
        let destination = native_address(&destination, self.network)?;
        let output = Output::new(destination, transfer.amount).map_err(transaction_error)?;
        Ok((transfer.wallet, source, output))
    }

    fn sources(&self, transfers: Vec<Transfer>) -> Result<Vec<Source>, SendError> {
        let mut sources: Vec<Source> = Vec::new();
        for (index, transfer) in transfers.into_iter().enumerate() {
            let (wallet, address, output) = self
                .parse(transfer)
                .map_err(|error| SendError::at(index, Vec::new(), error))?;
            if let Some(source) = sources.iter_mut().find(|source| source.address == address) {
                source.recipients.push(output);
            } else {
                sources.push(Source {
                    wallet,
                    address,
                    recipients: vec![output],
                });
            }
        }
        Ok(sources)
    }
}

impl Sender for Batch {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(async move {
            if transfers.is_empty() {
                return Err(failure("transaction batch is empty"));
            }
            let sources = self.sources(transfers)?;
            let fee_rate = self
                .fees
                .estimate(self.fee_target_blocks)
                .await
                .map_err(failure_with)?;
            if fee_rate > self.max_fee_rate {
                return Err(failure("estimated fee rate exceeds the configured maximum"));
            }

            let mut funding = Vec::with_capacity(sources.len());
            let mut owners = Vec::new();
            let mut checkpoint = None;
            for source in &sources {
                let set = self
                    .utxos
                    .utxos(vec![source.address.clone()])
                    .await
                    .map_err(failure_with)?;
                if checkpoint
                    .as_ref()
                    .is_some_and(|expected| expected != &set.checkpoint)
                {
                    return Err(failure(
                        "indexed output snapshot changed while building the transaction",
                    ));
                }
                checkpoint.get_or_insert(set.checkpoint);
                let available = set
                    .outputs
                    .into_iter()
                    .map(|output| {
                        SpendSource::from_exact_selection(
                            self.network,
                            &source.address,
                            TransactionId(output.transaction_id),
                            output.output_index,
                            output.value,
                            output.script_pubkey,
                        )
                        .map_err(failure_with)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                owners.extend(std::iter::repeat_n(source.wallet.as_ref(), available.len()));
                funding.push(Funding {
                    available,
                    recipients: source.recipients.clone(),
                    change_address: source.address.clone(),
                });
            }

            let signed = BatchBuilder::new(self.network, funding, fee_rate)
                .sign_each(&owners)
                .await
                .map_err(failure_with)?;
            let prepared = Prepared::new(
                PREPARED_KIND,
                BaseId::new(signed.id().to_string()),
                Envelope::new(signed.consensus_bytes().to_vec()),
            );
            let submitted = sources[0]
                .wallet
                .broadcaster()
                .broadcast(&prepared)
                .await
                .map_err(failure_with)?;
            Ok(vec![submitted.id])
        })
    }
}

fn native_address(address: &base::Address, network: Network) -> Result<Address, Error> {
    let value = std::str::from_utf8(address.as_bytes())
        .map_err(|_| transaction_error("Bitcoin address is not UTF-8"))?;
    Address::parse_for_network(value, network).map_err(transaction_error)
}

fn failure(message: &'static str) -> SendError {
    SendError::at(0, Vec::new(), Error::new(ErrorKind::Transaction, message))
}

fn failure_with(error: impl std::fmt::Display) -> SendError {
    SendError::at(0, Vec::new(), transaction_error(error))
}

fn transaction_error(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Transaction, error.to_string())
}
