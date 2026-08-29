use std::sync::Arc;

use base::{
    SignedTransaction as Prepared, TransactionEnvelope as Envelope, TransactionId as BaseId,
};
use wallets::{Error, ErrorKind, MAX_TRANSFERS, SendError, SendFuture, Sender, Transfer, Wallet};

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
                .map_err(|error| SendError::item(index, Vec::new(), error))?;
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
                return Err(SendError::collection(
                    ErrorKind::InvalidBatch,
                    "at least one transfer is required",
                ));
            }
            if transfers.len() > MAX_TRANSFERS {
                return Err(SendError::collection(
                    ErrorKind::InvalidBatch,
                    "at most 50 transfers are allowed",
                ));
            }
            let sources = self.sources(transfers)?;
            let fee_rate = self
                .fees
                .estimate(self.fee_target_blocks)
                .await
                .map_err(operation_failure_with)?;
            if fee_rate > self.max_fee_rate {
                return Err(operation_failure(
                    "estimated fee rate exceeds the configured maximum",
                ));
            }

            let mut funding = Vec::with_capacity(sources.len());
            let mut owners = Vec::new();
            let mut checkpoint = None;
            for source in &sources {
                let set = self
                    .utxos
                    .utxos(vec![source.address.clone()])
                    .await
                    .map_err(operation_failure_with)?;
                if checkpoint
                    .as_ref()
                    .is_some_and(|expected| expected != &set.checkpoint)
                {
                    return Err(operation_failure(
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
                        .map_err(operation_failure_with)
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
                .map_err(operation_failure_with)?;
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
                .map_err(grouped_failure)?;
            Ok(vec![submitted.id])
        })
    }
}

fn native_address(address: &base::Address, network: Network) -> Result<Address, Error> {
    let value = std::str::from_utf8(address.as_bytes())
        .map_err(|_| transaction_error("Bitcoin address is not UTF-8"))?;
    Address::parse_for_network(value, network).map_err(transaction_error)
}

fn operation_failure(message: &'static str) -> SendError {
    SendError::operation(ErrorKind::Transaction, message)
}

fn operation_failure_with(error: impl std::fmt::Display) -> SendError {
    SendError::operation(ErrorKind::Transaction, error.to_string())
}

fn grouped_failure(error: base::TransactionError) -> SendError {
    SendError::grouped(Vec::new(), error.into())
}

fn transaction_error(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Transaction, error.to_string())
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction as NativeTransaction, TxIn, TxOut, Txid,
        Witness, absolute, consensus, hashes::Hash, transaction::Version,
    };
    use futures_executor::block_on;
    use indexing::{
        BoxFuture, ChainId, History, HistoryQuery, IndexError, IndexScope, OutputPage,
        OutputRequest, Outputs, SourceError, TransactionPage,
    };
    use wallets::{AddressText, Provider, SecretBytes};

    use super::*;
    use crate::{AddressType, Transactions, WalletConfig, WalletProvider};

    struct InactiveDependencies;

    impl Outputs for InactiveDependencies {
        fn list<'a>(
            &'a self,
            _request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async { unreachable!("invalid batch must not read indexed outputs") })
        }
    }

    impl Fees for InactiveDependencies {
        fn estimate<'a>(
            &'a self,
            _target_blocks: u16,
        ) -> BoxFuture<'a, Result<FeeRate, SourceError>> {
            Box::pin(async { unreachable!("invalid batch must not estimate fees") })
        }
    }

    impl Transactions for InactiveDependencies {
        fn preflight<'a>(
            &'a self,
            _transaction: &'a crate::SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::Preflight, SourceError>> {
            Box::pin(async { unreachable!("invalid batch must not preflight a transaction") })
        }

        fn broadcast<'a>(
            &'a self,
            _transaction: crate::SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<TransactionId, base::TransactionError>> {
            Box::pin(async { unreachable!("invalid batch must not broadcast a transaction") })
        }
    }

    impl History for InactiveDependencies {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async { unreachable!("invalid batch must not read indexed history") })
        }
    }

    fn direct_sender() -> (Arc<dyn Sender>, Arc<dyn Wallet>) {
        let network = Network::Regtest;
        let scope = IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: network.canonical_name().to_owned(),
        };
        let dependencies = Arc::new(InactiveDependencies);
        let outputs: Arc<dyn Outputs> = dependencies.clone();
        let fees: Arc<dyn Fees> = dependencies.clone();
        let transactions: Arc<dyn Transactions> = dependencies.clone();
        let history: Arc<dyn History> = dependencies;
        let utxos = Arc::new(
            IndexUtxos::new(scope.clone(), network, outputs)
                .expect("fixture scope must match the Bitcoin network"),
        );
        let provider = WalletProvider::new(
            WalletConfig {
                scope,
                network,
                address_type: AddressType::SegwitV0,
                fee_target_blocks: 6,
                max_fee_rate: FeeRate::new(1_000),
            },
            utxos,
            fees,
            transactions,
            history,
        );
        let wallet = block_on(provider.create(SecretBytes::new([1_u8; 32])))
            .expect("fixed valid secret must create a Bitcoin wallet");
        (provider.transactions(), wallet)
    }

    fn assert_invalid_batch(failure: SendError, message: &str) {
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.source.kind, ErrorKind::InvalidBatch);
        assert_eq!(failure.source.message, message);
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(failure.to_string(), message);
    }

    fn exact_envelope_id() -> BaseId {
        let transaction = NativeTransaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([3; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let native_id = TransactionId::from(transaction.compute_txid());
        let signed = crate::SignedTransaction::from_consensus_bytes(
            native_id,
            consensus::serialize(&transaction),
        )
        .expect("fixture envelope and transaction ID must agree");
        BaseId::new(signed.id().to_string())
    }

    #[test]
    fn direct_sender_rejects_an_empty_batch_before_chain_io() {
        let (sender, _) = direct_sender();

        let failure = block_on(sender.send(Vec::new()))
            .expect_err("the concrete Bitcoin sender must reject an empty batch");

        assert_invalid_batch(failure, "at least one transfer is required");
    }

    #[test]
    fn direct_sender_rejects_51_items_before_chain_io() {
        let (sender, wallet) = direct_sender();
        let destination = wallet
            .address_text(&wallet.address())
            .expect("fixture wallet address must format");
        let amount: base::Decimal = "0.00000001"
            .parse()
            .expect("one satoshi must be a valid decimal");
        let transfers = (0..=MAX_TRANSFERS)
            .map(|_| Transfer {
                wallet: wallet.clone(),
                to: AddressText::new(destination.encoding, destination.text.clone()),
                amount: amount.clone(),
            })
            .collect();

        let failure = block_on(sender.send(transfers))
            .expect_err("the concrete Bitcoin sender must reject 51 items");

        assert_invalid_batch(failure, "at most 50 transfers are allowed");
    }

    #[test]
    fn operation_failures_are_index_free() {
        for failure in [
            operation_failure("fee ceiling exceeded"),
            operation_failure_with("indexed outputs unavailable"),
        ] {
            assert!(failure.accepted.is_empty());
            assert_eq!(failure.failed_index, None);
            assert_eq!(failure.ambiguous_transaction_id, None);
            assert_eq!(failure.source.ambiguous_transaction_id, None);
        }
    }

    #[test]
    fn grouped_broadcast_failure_preserves_exact_ambiguity_without_an_index() {
        let ambiguous = exact_envelope_id();
        let failure = grouped_failure(
            base::TransactionError::new(
                base::TransactionErrorKind::Timeout,
                "grouped submission outcome is unknown",
            )
            .with_ambiguous_transaction_id(ambiguous.clone()),
        );

        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, Some(ambiguous));
        assert_eq!(failure.source.ambiguous_transaction_id, None);
    }

    #[test]
    fn grouped_broadcast_failure_does_not_parse_provider_prose_as_ambiguity() {
        let failure = grouped_failure(base::TransactionError::new(
            base::TransactionErrorKind::Unavailable,
            format!("provider claimed transaction {:064x}", 9),
        ));

        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.source.ambiguous_transaction_id, None);
    }
}
