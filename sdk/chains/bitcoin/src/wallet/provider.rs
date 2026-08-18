use std::sync::Arc;

use base::{
    Address as BaseAddress, Addresser, Broadcaster, BuilderCast, Decimal, InputPolicy, KeyPair,
    Submission as BroadcastReceipt, TransactionBuilder as BaseBuilder, TransactionEnvelope,
    TransactionError, TransactionErrorKind, TransactionFuture, TransactionId as BaseTransactionId,
    TransactionSnapshot, UtxoBuilder,
};
use bitcoin::{
    Address as NativeAddress, CompressedPublicKey, PublicKey, XOnlyPublicKey, secp256k1::Secp256k1,
};
use crypto::{PublicKeyFormat, SecretKey};
use indexing::{History as IndexHistory, IndexScope};
use wallets::{
    AddressEncoding, AddressFormat as WalletAddressFormat, AddressText, Balance, BalanceReader,
    Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, Provider, SecretBytes,
    TransactionFactory, Wallet as WalletContract,
};

use crate::{
    Address, BuildRequest, FeeRate, Fees, Network, Output, Satoshi, SignedTransaction, SpendSource,
    TransactionBuilder, TransactionId, Transactions, Utxos,
};

const SNAPSHOT_KIND: &str = "bitcoin.transfer.v1";
const PREPARED_KIND: &str = "bitcoin.signed.v1";

mod collector;
mod history;
mod restore;

use collector::BatchCollector;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressType {
    SegwitV0,
    Taproot,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub scope: IndexScope,
    pub network: Network,
    pub address_type: AddressType,
    pub fee_target_blocks: u16,
    pub max_fee_rate: FeeRate,
}

impl Config {
    fn validate(&self) -> Result<(), WalletError> {
        if self.scope.chain.0 != "bitcoin" || self.scope.network != network_name(self.network) {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "Bitcoin wallet chain and network must agree",
            ));
        }
        Ok(())
    }
}

pub struct Factory {
    config: Config,
    utxos: Arc<dyn Utxos>,
    fees: Arc<dyn Fees>,
    transactions: Arc<dyn Transactions>,
    history: Arc<dyn IndexHistory>,
}

impl Factory {
    #[must_use]
    pub fn new(
        config: Config,
        utxos: Arc<dyn Utxos>,
        fees: Arc<dyn Fees>,
        transactions: Arc<dyn Transactions>,
        history: Arc<dyn IndexHistory>,
    ) -> Self {
        Self {
            config,
            utxos,
            fees,
            transactions,
            history,
        }
    }
}

impl Provider for Factory {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn WalletContract>> {
        Box::pin(async move {
            self.config.validate()?;
            let temporary = SecretKey::new(secret.as_bytes().to_vec())
                .map_err(|error| map_error(WalletErrorKind::InvalidSecret, error))?;
            let format = match self.config.address_type {
                AddressType::SegwitV0 => PublicKeyFormat::Compressed,
                AddressType::Taproot => PublicKeyFormat::XOnly,
            };
            let public = temporary
                .public_key(format)
                .map_err(|error| map_error(WalletErrorKind::InvalidSecret, error))?;
            let native = match self.config.address_type {
                AddressType::SegwitV0 => {
                    let key = PublicKey::from_slice(&public.bytes)
                        .map_err(|error| map_error(WalletErrorKind::InvalidSecret, error))?;
                    let key = CompressedPublicKey::try_from(key)
                        .map_err(|error| map_error(WalletErrorKind::InvalidSecret, error))?;
                    NativeAddress::p2wpkh(&key, self.config.network.native())
                }
                AddressType::Taproot => {
                    let key = XOnlyPublicKey::from_slice(&public.bytes)
                        .map_err(|error| map_error(WalletErrorKind::InvalidSecret, error))?;
                    NativeAddress::p2tr(
                        &Secp256k1::verification_only(),
                        key,
                        None,
                        self.config.network.native(),
                    )
                }
            };
            let address = Address::from_encoded(native.to_string());
            let signer = KeyPair::new(address.clone(), secret.as_bytes().to_vec())
                .map_err(|error| map_error(WalletErrorKind::InvalidSecret, error))?;
            Ok(Arc::new(Wallet {
                config: self.config.clone(),
                address,
                signer: Arc::new(signer),
                utxos: self.utxos.clone(),
                fees: self.fees.clone(),
                transactions: self.transactions.clone(),
                history: self.history.clone(),
            }) as Arc<dyn WalletContract>)
        })
    }
}

struct Wallet {
    config: Config,
    address: Address,
    signer: Arc<KeyPair<Address>>,
    utxos: Arc<dyn Utxos>,
    fees: Arc<dyn Fees>,
    transactions: Arc<dyn Transactions>,
    history: Arc<dyn IndexHistory>,
}

impl Wallet {
    fn builder(&self) -> Builder {
        Builder {
            scope: self.config.scope.clone(),
            network: self.config.network,
            source: self.address.clone(),
            change: self.address.clone(),
            signer: self.signer.clone(),
            utxos: self.utxos.clone(),
            fees: self.fees.clone(),
            fee_target_blocks: self.config.fee_target_blocks,
            max_fee_rate: self.config.max_fee_rate,
            input_policy: InputPolicy::Automatic,
            recipients: Vec::new(),
        }
    }
}

impl Addresser for Wallet {
    fn address(&self) -> BaseAddress {
        self.address.address()
    }
}

impl base::Signer for Wallet {
    fn sign<'a>(&'a self, request: base::SignRequest) -> base::SignFuture<'a> {
        self.signer.sign(request)
    }
}

impl WalletAddressFormat for Wallet {
    fn address_text(&self, address: &BaseAddress) -> Result<AddressText, WalletError> {
        let text = std::str::from_utf8(address.as_bytes()).map_err(|_| {
            WalletError::new(
                WalletErrorKind::InvalidAddress,
                "Bitcoin address bytes are not UTF-8",
            )
        })?;
        let parsed = text
            .parse::<NativeAddress<bitcoin::address::NetworkUnchecked>>()
            .map_err(|error| map_error(WalletErrorKind::InvalidAddress, error))?
            .require_network(self.config.network.native())
            .map_err(|error| map_error(WalletErrorKind::InvalidAddress, error))?;
        let encoding = match parsed.address_type() {
            Some(bitcoin::AddressType::P2pkh | bitcoin::AddressType::P2sh) => {
                AddressEncoding::Base58Check
            }
            Some(bitcoin::AddressType::P2tr | bitcoin::AddressType::P2a) => {
                AddressEncoding::Bech32m
            }
            Some(bitcoin::AddressType::P2wpkh | bitcoin::AddressType::P2wsh) => {
                AddressEncoding::Bech32
            }
            None | Some(_) => {
                return Err(WalletError::new(
                    WalletErrorKind::InvalidAddress,
                    "Bitcoin address type is unsupported",
                ));
            }
        };
        Ok(AddressText::new(encoding, parsed.to_string()))
    }

    fn parse_address(&self, address: &AddressText) -> Result<BaseAddress, WalletError> {
        let parsed = Address::parse_for_network(&address.text, self.config.network)
            .map_err(|error| map_error(WalletErrorKind::InvalidAddress, error))?;
        let canonical = self.address_text(&parsed.address())?;
        if canonical.encoding != address.encoding {
            return Err(WalletError::new(
                WalletErrorKind::InvalidAddress,
                "Bitcoin address encoding does not match its text",
            ));
        }
        Ok(parsed.address())
    }
}

impl BalanceReader for Wallet {
    fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
        Box::pin(async move {
            let set = self
                .utxos
                .utxos(vec![self.address.clone()])
                .await
                .map_err(|error| map_error(WalletErrorKind::Balance, error))?;
            let atomic = set.outputs.iter().try_fold(0_u64, |sum, output| {
                sum.checked_add(output.value.0).ok_or_else(|| {
                    WalletError::new(WalletErrorKind::Balance, "Bitcoin balance exceeds u64")
                })
            })?;
            Ok(Balance {
                amount: Satoshi(atomic).decimal(),
                observed_at: Some(set.checkpoint),
            })
        })
    }
}

impl TransactionFactory for Wallet {
    fn transaction(&self) -> Box<dyn BaseBuilder> {
        Box::new(self.builder())
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

impl wallets::CollectionFactory for Wallet {
    fn collector(&self) -> Option<Box<dyn wallets::Collector>> {
        Some(Box::new(BatchCollector::new(
            self.config.scope.clone(),
            self.config.network,
            self.utxos.clone(),
            self.fees.clone(),
            self.config.fee_target_blocks,
            self.config.max_fee_rate,
        )))
    }
}

impl wallets::Sweeper for Wallet {}

struct Builder {
    scope: IndexScope,
    network: Network,
    source: Address,
    change: Address,
    signer: Arc<KeyPair<Address>>,
    utxos: Arc<dyn Utxos>,
    fees: Arc<dyn Fees>,
    fee_target_blocks: u16,
    max_fee_rate: FeeRate,
    input_policy: InputPolicy,
    recipients: Vec<(Address, Decimal)>,
}

impl Builder {
    fn validate(&self) -> Result<(), TransactionError> {
        if self.scope.chain.0 != "bitcoin" || self.scope.network != network_name(self.network) {
            return Err(transaction_error(
                TransactionErrorKind::InvalidSnapshot,
                "Bitcoin transaction identity, chain, and network do not agree",
            ));
        }
        Address::parse_for_network(self.source.encoded(), self.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        Address::parse_for_network(self.change.encoded(), self.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        Ok(())
    }
}

impl BuilderCast for Builder {
    fn utxo(&mut self) -> Option<&mut dyn UtxoBuilder> {
        Some(self)
    }
}

impl UtxoBuilder for Builder {
    fn inputs(&mut self, policy: InputPolicy) -> Result<(), TransactionError> {
        self.input_policy = policy;
        Ok(())
    }

    fn change(&mut self, address: BaseAddress) -> Result<(), TransactionError> {
        let value = std::str::from_utf8(address.as_bytes()).map_err(|_| {
            transaction_error(
                TransactionErrorKind::InvalidAddress,
                "Bitcoin address is not UTF-8",
            )
        })?;
        self.change = Address::parse_for_network(value, self.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAddress, error))?;
        Ok(())
    }
}

impl BaseBuilder for Builder {
    fn transfer(
        &mut self,
        destination: BaseAddress,
        amount: Decimal,
    ) -> Result<(), TransactionError> {
        let value = std::str::from_utf8(destination.as_bytes()).map_err(|_| {
            transaction_error(
                TransactionErrorKind::InvalidAddress,
                "Bitcoin address is not UTF-8",
            )
        })?;
        let address = Address::parse_for_network(value, self.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAddress, error))?;
        Satoshi::from_decimal(&amount)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAmount, error))?;
        self.recipients.push((address, amount));
        Ok(())
    }

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        self.validate()?;
        if self.recipients.is_empty() {
            return Err(transaction_error(
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
            ));
        }
        let transfers = self
            .recipients
            .iter()
            .map(|(destination, amount)| {
                serde_json::json!({
                    "destination": destination.to_string(),
                    "amount": amount.to_string(),
                })
            })
            .collect::<Vec<_>>();
        Ok(TransactionSnapshot::new(
            SNAPSHOT_KIND,
            serde_json::json!({
                "scope": {
                    "chain": self.scope.chain.0.as_str(),
                    "network": self.scope.network.as_str(),
                },
                "source": self.source.to_string(),
                "asset": {
                    "kind": "native",
                    "ticker": crate::BTC.ticker,
                    "decimals": crate::BTC.decimals,
                },
                "transfers": transfers,
                "inputs": match self.input_policy {
                    InputPolicy::Automatic => "automatic",
                    InputPolicy::SpendAll => "all",
                },
                "change": self.change.to_string(),
            }),
        ))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            self.validate()?;
            if self.recipients.is_empty() {
                return Err(transaction_error(
                    TransactionErrorKind::InvalidTransaction,
                    "transaction has no recipients",
                ));
            }
            let set = self
                .utxos
                .utxos(vec![self.source.clone()])
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            let available = set
                .outputs
                .into_iter()
                .map(|output| {
                    SpendSource::from_exact_selection(
                        self.network,
                        &self.source,
                        TransactionId(output.transaction_id),
                        output.output_index,
                        output.value,
                        output.script_pubkey,
                    )
                    .map_err(|error| {
                        transaction_error(TransactionErrorKind::InvalidTransaction, error)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let recipients = self
                .recipients
                .iter()
                .cloned()
                .map(|(address, amount)| {
                    Output::new(address, amount).map_err(|error| {
                        transaction_error(TransactionErrorKind::InvalidAmount, error)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let fee_rate = self
                .fees
                .estimate(self.fee_target_blocks)
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            if fee_rate > self.max_fee_rate {
                return Err(transaction_error(
                    TransactionErrorKind::Fee,
                    "estimated Bitcoin fee rate exceeds the configured maximum",
                ));
            }
            let request = BuildRequest {
                available,
                recipients,
                change_address: self.change.clone(),
                fee_rate,
                drain_wallet: self.input_policy == InputPolicy::SpendAll,
            };
            let signed = TransactionBuilder::new(self.network, request)
                .sign(self.signer.as_ref())
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Signing, error))?;
            Ok(base::SignedTransaction::new(
                PREPARED_KIND,
                BaseTransactionId::new(signed.id().to_string()),
                TransactionEnvelope::new(signed.consensus_bytes().to_vec()),
            ))
        })
    }
}

const fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet3 => "testnet3",
        Network::Testnet4 => "testnet4",
        Network::Signet => "signet",
        Network::Regtest => "regtest",
    }
}

impl Broadcaster for Wallet {
    fn broadcast<'a>(
        &'a self,
        prepared: &'a base::SignedTransaction,
    ) -> TransactionFuture<'a, Result<BroadcastReceipt, TransactionError>> {
        Box::pin(async move {
            if prepared.version() != base::SignedTransaction::VERSION
                || prepared.kind() != PREPARED_KIND
            {
                return Err(transaction_error(
                    TransactionErrorKind::InvalidTransaction,
                    "prepared transaction is not a Bitcoin signed envelope",
                ));
            }
            let id = prepared.id().as_str().parse().map_err(|error| {
                transaction_error(TransactionErrorKind::InvalidTransaction, error)
            })?;
            let signed = SignedTransaction::from_consensus_bytes(
                id,
                prepared.envelope().as_bytes().to_vec(),
            )
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidTransaction, error))?;
            let preflight = self
                .transactions
                .preflight(&signed, self.config.max_fee_rate)
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            if !preflight.allowed {
                return Err(transaction_error(
                    TransactionErrorKind::Rejected,
                    "Bitcoin node rejected transaction preflight",
                ));
            }
            let id = self
                .transactions
                .broadcast(signed, self.config.max_fee_rate)
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            Ok(BroadcastReceipt {
                id: BaseTransactionId::new(id.to_string()),
            })
        })
    }
}
pub(super) fn transaction_error(
    kind: TransactionErrorKind,
    error: impl std::fmt::Display,
) -> TransactionError {
    TransactionError::new(kind, error.to_string())
}

fn map_error(kind: WalletErrorKind, error: impl std::fmt::Display) -> WalletError {
    WalletError::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Mutex};

    use indexing::{
        AssetId, BlockHash, BlockHeight, BlockRef, BoxFuture, ChainId, HistoryQuery, IndexError,
        IndexedOutput, ObservedTransaction, OutputId, OutputPage, OutputQuery, OutputRequest,
        OutputSnapshot, RebuildGeneration, SourceError, TransactionPage, TransactionQuery,
        TransactionRef,
    };
    use wallets::Wallets;

    use super::*;
    use crate::{IndexUtxos, Preflight};

    enum Rpc {
        Test,
    }

    enum Outputs {
        Test,
    }

    type CatalogEntry = ([u8; 32], u32, u64);

    #[derive(Default)]
    struct Catalog {
        outputs: Mutex<BTreeMap<String, Vec<CatalogEntry>>>,
    }

    impl OutputQuery for Outputs {
        fn outputs<'a>(
            &'a self,
            request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async move {
                let address = Address::parse_for_network(&request.address.value, Network::Regtest)
                    .expect("wallet requests its canonical regtest address");
                Ok(OutputPage {
                    snapshot: OutputSnapshot {
                        generation: RebuildGeneration(0),
                        revision: 1,
                        checkpoint: Some(BlockRef {
                            height: BlockHeight(10),
                            hash: BlockHash(vec![3; 32]),
                            parent_hash: None,
                            timestamp: None,
                        }),
                    },
                    outputs: vec![IndexedOutput {
                        id: OutputId {
                            transaction: TransactionRef {
                                scope: request.scope.clone(),
                                value: TransactionId([4; 32]).to_string(),
                            },
                            index: 0,
                        },
                        address: request.address,
                        asset: AssetId {
                            chain: request.scope.chain,
                            asset: "native".to_owned(),
                        },
                        // Indexing stores chain-native atomic units (satoshis),
                        // while the wallet exposes the resulting balance as BTC.
                        amount: Decimal::from(100_000_u64),
                        evidence: address
                            .script_pubkey_for_network(Network::Regtest)
                            .expect("fixture address must match regtest")
                            .into_bytes(),
                        created_at: BlockHeight(6),
                        coinbase: false,
                    }],
                    next: None,
                })
            })
        }
    }

    impl OutputQuery for Catalog {
        fn outputs<'a>(
            &'a self,
            request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async move {
                let address = Address::parse_for_network(&request.address.value, Network::Regtest)
                    .expect("collector requests a canonical regtest address");
                let entries = self
                    .outputs
                    .lock()
                    .expect("catalog lock")
                    .get(&request.address.value)
                    .cloned()
                    .unwrap_or_default();
                let outputs = entries
                    .into_iter()
                    .map(|(transaction_id, index, amount)| IndexedOutput {
                        id: OutputId {
                            transaction: TransactionRef {
                                scope: request.scope.clone(),
                                value: TransactionId(transaction_id).to_string(),
                            },
                            index,
                        },
                        address: request.address.clone(),
                        asset: AssetId {
                            chain: request.scope.chain.clone(),
                            asset: "native".to_owned(),
                        },
                        amount: Decimal::from(amount),
                        evidence: address
                            .script_pubkey_for_network(Network::Regtest)
                            .expect("fixture address must match regtest")
                            .into_bytes(),
                        created_at: BlockHeight(6),
                        coinbase: false,
                    })
                    .collect();
                Ok(OutputPage {
                    snapshot: OutputSnapshot {
                        generation: RebuildGeneration(0),
                        revision: 1,
                        checkpoint: Some(BlockRef {
                            height: BlockHeight(10),
                            hash: BlockHash(vec![3; 32]),
                            parent_hash: None,
                            timestamp: None,
                        }),
                    },
                    outputs,
                    next: None,
                })
            })
        }
    }

    impl Fees for Rpc {
        fn estimate<'a>(
            &'a self,
            _target_blocks: u16,
        ) -> crate::BoxFuture<'a, Result<FeeRate, SourceError>> {
            Box::pin(async { Ok(FeeRate::new(1_000)) })
        }
    }

    impl Transactions for Rpc {
        fn preflight<'a>(
            &'a self,
            transaction: &'a SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> crate::BoxFuture<'a, Result<Preflight, SourceError>> {
            let virtual_size = transaction
                .virtual_size()
                .expect("signed fixture must decode");
            Box::pin(async move {
                Ok(Preflight {
                    allowed: true,
                    reject_reason: None,
                    virtual_size: Some(virtual_size),
                    base_fee: None,
                })
            })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> crate::BoxFuture<'a, Result<TransactionId, SourceError>> {
            Box::pin(async move { Ok(transaction.id()) })
        }

        fn receipt<'a>(
            &'a self,
            id: &'a TransactionId,
        ) -> crate::BoxFuture<'a, Result<Option<crate::Receipt>, SourceError>> {
            let id = *id;
            Box::pin(async move {
                Ok(Some(crate::Receipt {
                    id,
                    included_in: None,
                    confirmations: 2,
                    replaced_by: None,
                }))
            })
        }
    }

    enum EmptyHistory {
        Fixture,
    }

    impl IndexHistory for EmptyHistory {
        fn transaction<'a>(
            &'a self,
            _request: TransactionQuery,
        ) -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>> {
            Box::pin(async { Ok(None) })
        }

        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async {
                Ok(TransactionPage {
                    transactions: Vec::new(),
                    next: None,
                })
            })
        }
    }

    fn kind() -> &'static str {
        "regtest-native"
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "regtest".to_owned(),
        }
    }

    fn selection(byte: u8, index: u32, amount: u64) -> wallets::SelectedOutput {
        wallets::SelectedOutput {
            output: OutputId {
                transaction: TransactionRef {
                    scope: scope(),
                    value: TransactionId([byte; 32]).to_string(),
                },
                index,
            },
            amount: Decimal::from(amount),
        }
    }

    async fn collection_wallets(
        catalog: Arc<Catalog>,
    ) -> (Arc<dyn WalletContract>, Arc<dyn WalletContract>) {
        let rpc = Arc::new(Rpc::Test);
        let utxos = Arc::new(
            IndexUtxos::new(scope(), Network::Regtest, catalog)
                .expect("fixture scope must configure indexed outputs"),
        );
        let provider = Factory::new(
            Config {
                scope: scope(),
                network: Network::Regtest,
                address_type: AddressType::SegwitV0,
                fee_target_blocks: 2,
                max_fee_rate: FeeRate::new(10_000),
            },
            utxos,
            rpc.clone(),
            rpc,
            Arc::new(EmptyHistory::Fixture),
        );
        let first = provider
            .create(SecretBytes::new([1_u8; 32]))
            .await
            .expect("first key must create a wallet");
        let second = provider
            .create(SecretBytes::new([2_u8; 32]))
            .await
            .expect("second key must create a wallet");
        (first, second)
    }

    fn encoded(wallet: &dyn WalletContract) -> String {
        std::str::from_utf8(wallet.address().as_bytes())
            .expect("Bitcoin address is encoded text")
            .to_owned()
    }

    #[tokio::test]
    async fn collector_signs_exact_multi_owner_inputs() {
        let catalog = Arc::new(Catalog::default());
        let (first, second) = collection_wallets(catalog.clone()).await;
        catalog.outputs.lock().expect("catalog lock").extend([
            (
                encoded(first.as_ref()),
                vec![([3; 32], 1, 70_000), ([1; 32], 0, 50_000)],
            ),
            (encoded(second.as_ref()), vec![([2; 32], 4, 80_000)]),
        ]);

        let mut collector = first.collector().expect("Bitcoin supports collection");
        collector
            .source(
                first.clone(),
                vec![selection(3, 1, 70_000), selection(1, 0, 50_000)],
            )
            .expect("first source must configure");
        collector
            .source(second.clone(), vec![selection(2, 4, 80_000)])
            .expect("second source must configure");
        collector
            .destination(first.address())
            .expect("destination must configure");
        let prepared = collector.prepare().await.expect("batch must sign");
        let id = prepared
            .transaction
            .id()
            .as_str()
            .parse()
            .expect("canonical txid");
        let signed = SignedTransaction::from_consensus_bytes(
            id,
            prepared.transaction.envelope().as_bytes().to_vec(),
        )
        .expect("returned envelope and ID must agree");
        let inspected = signed.inspect().expect("signed batch must inspect");
        assert_eq!(inspected.inputs.len(), 3);
        assert_eq!(inspected.outputs.len(), 1);
        assert!(inspected.outputs[0].value.0 < 200_000);
        assert!(inspected.outputs[0].value.0 > 0);
        assert_eq!(
            prepared.transaction.id().as_str(),
            inspected.transaction_id.to_string()
        );
        assert_eq!(
            inspected
                .inputs
                .iter()
                .map(|input| (
                    input.outpoint.transaction_id.0[0],
                    input.outpoint.output_index
                ))
                .collect::<Vec<_>>(),
            vec![(1, 0), (2, 4), (3, 1)]
        );
    }

    #[tokio::test]
    async fn collector_rejects_duplicate_amount_drift_and_wrong_owner() {
        let catalog = Arc::new(Catalog::default());
        let (first, second) = collection_wallets(catalog.clone()).await;
        catalog.outputs.lock().expect("catalog lock").extend([
            (encoded(first.as_ref()), vec![([1; 32], 0, 50_000)]),
            (encoded(second.as_ref()), vec![([2; 32], 0, 60_000)]),
        ]);

        let mut duplicate = first.collector().expect("Bitcoin supports collection");
        let selected = selection(1, 0, 50_000);
        let error = duplicate
            .source(first.clone(), vec![selected.clone(), selected])
            .expect_err("duplicate exact outputs must fail");
        assert_eq!(error.kind, WalletErrorKind::Transaction);

        let mut drift = first.collector().expect("Bitcoin supports collection");
        drift
            .source(first.clone(), vec![selection(1, 0, 49_999)])
            .expect("amount fence is checked against canonical output at prepare");
        drift.destination(first.address()).expect("destination");
        let error = drift
            .prepare()
            .await
            .expect_err("reservation amount drift must fail");
        assert!(error.message.contains("amount changed"));

        let mut wrong_owner = first.collector().expect("Bitcoin supports collection");
        wrong_owner
            .source(second, vec![selection(1, 0, 50_000)])
            .expect("ownership is checked against canonical outputs at prepare");
        wrong_owner
            .destination(first.address())
            .expect("destination");
        let error = wrong_owner
            .prepare()
            .await
            .expect_err("another wallet must not spend the selected output");
        assert!(error.message.contains("not spendable"));
    }

    #[tokio::test]
    async fn collector_rejects_a_wallet_from_another_network() {
        let catalog = Arc::new(Catalog::default());
        let (collector_wallet, _) = collection_wallets(catalog.clone()).await;
        let rpc = Arc::new(Rpc::Test);
        let utxos = Arc::new(
            IndexUtxos::new(scope(), Network::Regtest, catalog)
                .expect("fixture output source must configure"),
        );
        let foreign = Factory::new(
            Config {
                scope: IndexScope {
                    chain: ChainId(crate::CHAIN.to_owned()),
                    network: "mainnet".to_owned(),
                },
                network: Network::Mainnet,
                address_type: AddressType::SegwitV0,
                fee_target_blocks: 2,
                max_fee_rate: FeeRate::new(10_000),
            },
            utxos,
            rpc.clone(),
            rpc,
            Arc::new(EmptyHistory::Fixture),
        )
        .create(SecretBytes::new([3_u8; 32]))
        .await
        .expect("valid mainnet key must create a wallet");

        let mut collector = collector_wallet
            .collector()
            .expect("Bitcoin supports collection");
        let error = collector
            .source(foreign, vec![selection(1, 0, 50_000)])
            .expect_err("a source from another network must fail");
        assert_eq!(error.kind, WalletErrorKind::InvalidAddress);
    }

    #[tokio::test]
    async fn prepares_and_broadcasts_through_only_wallet_abstractions() {
        let rpc = Arc::new(Rpc::Test);
        let utxos = Arc::new(
            IndexUtxos::new(scope(), Network::Regtest, Arc::new(Outputs::Test))
                .expect("fixture scope must configure indexed outputs"),
        );
        let mut wallets = Wallets::new();
        wallets
            .register(
                kind(),
                Factory::new(
                    Config {
                        scope: scope(),
                        network: Network::Regtest,
                        address_type: AddressType::SegwitV0,
                        fee_target_blocks: 2,
                        max_fee_rate: FeeRate::new(10_000),
                    },
                    utxos,
                    rpc.clone(),
                    rpc,
                    Arc::new(EmptyHistory::Fixture),
                ),
            )
            .expect("wallet key must be unique");
        let wallet = wallets
            .new_wallet(&kind(), SecretBytes::new([1_u8; 32]))
            .await
            .expect("valid local key must create the wallet");

        let address = wallet
            .address_text(&wallet.address())
            .expect("wallet address must have canonical text");
        assert_eq!(address.encoding, wallets::AddressEncoding::Bech32);
        assert!(address.text.starts_with("bcrt1q"));
        assert_eq!(
            wallet
                .parse_address(&address)
                .expect("canonical address text must round-trip"),
            wallet.address()
        );
        let display = wallet
            .display_amount(&Decimal::from(100_000_000_u64))
            .expect("atomic satoshis must convert exactly");
        assert_eq!(display.to_string(), "1");
        assert_eq!(
            display.to_atomic_u64(8).expect("BTC must round-trip"),
            100_000_000
        );

        assert_eq!(
            wallet
                .balance()
                .await
                .expect("balance must load")
                .amount
                .to_string(),
            "0.001"
        );
        let mut builder = wallet.transaction();
        builder
            .transfer(
                wallet.address(),
                "0.0005".parse::<Decimal>().expect("amount"),
            )
            .expect("transfer must configure");
        let snapshot = builder
            .snapshot()
            .expect("configured transfer must snapshot");
        assert!(snapshot.value().get("wallet").is_none());
        assert_eq!(snapshot.value()["scope"]["chain"], "bitcoin");
        assert_eq!(snapshot.value()["scope"]["network"], "regtest");
        let encoded = std::str::from_utf8(wallet.address().as_bytes())
            .expect("Bitcoin wallet address must be encoded text")
            .to_owned();
        assert_eq!(snapshot.value()["source"], encoded);
        assert_eq!(snapshot.value()["change"], encoded);
        assert_eq!(snapshot.value()["asset"]["kind"], "native");
        assert_eq!(snapshot.value()["asset"]["decimals"], 8);
        let json = serde_json::to_vec(&snapshot).expect("snapshot must serialize");
        let decoded: TransactionSnapshot =
            serde_json::from_slice(&json).expect("snapshot must deserialize");
        let restored = wallet
            .restore(&decoded)
            .expect("this wallet must restore its snapshot");
        assert_eq!(restored.snapshot().expect("restored state"), snapshot);
        let prepared = builder.prepare().await.expect("transaction must sign");
        let submission = wallet
            .broadcaster()
            .broadcast(&prepared)
            .await
            .expect("transaction must submit");
        assert_eq!(submission.id, prepared.id().clone());
    }

    #[tokio::test]
    async fn restore_rejects_another_wallet_network_or_asset() {
        let rpc = Arc::new(Rpc::Test);
        let utxos = Arc::new(
            IndexUtxos::new(scope(), Network::Regtest, Arc::new(Outputs::Test))
                .expect("fixture scope must configure indexed outputs"),
        );
        let mut wallets = Wallets::new();
        wallets
            .register(
                kind(),
                Factory::new(
                    Config {
                        scope: scope(),
                        network: Network::Regtest,
                        address_type: AddressType::SegwitV0,
                        fee_target_blocks: 2,
                        max_fee_rate: FeeRate::new(10_000),
                    },
                    utxos,
                    rpc.clone(),
                    rpc,
                    Arc::new(EmptyHistory::Fixture),
                ),
            )
            .expect("wallet key must be unique");
        let wallet = wallets
            .new_wallet(&kind(), SecretBytes::new([1_u8; 32]))
            .await
            .expect("valid local key must create the wallet");
        let mut builder = wallet.transaction();
        builder
            .transfer(
                wallet.address(),
                "0.0005".parse::<Decimal>().expect("amount"),
            )
            .expect("transfer must configure");
        let snapshot = builder.snapshot().expect("transaction must snapshot");

        for (field, value) in [
            ("network", serde_json::json!("mainnet")),
            ("asset", serde_json::json!("not-btc")),
        ] {
            let mut changed = snapshot.value().clone();
            match field {
                "network" => changed["scope"]["network"] = value,
                "asset" => changed["asset"]["ticker"] = value,
                _ => unreachable!("fixture enumerates known fields"),
            }
            let error = wallet
                .restore(&TransactionSnapshot::new(SNAPSHOT_KIND, changed))
                .err()
                .expect("foreign snapshot must fail");
            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot);
        }
    }

    #[tokio::test]
    async fn rejects_provider_scope_that_disagrees_with_the_bitcoin_network() {
        let rpc = Arc::new(Rpc::Test);
        let provider = Factory::new(
            Config {
                scope: IndexScope {
                    chain: ChainId(crate::CHAIN.to_owned()),
                    network: "mainnet".to_owned(),
                },
                network: Network::Regtest,
                address_type: AddressType::SegwitV0,
                fee_target_blocks: 2,
                max_fee_rate: FeeRate::new(10_000),
            },
            Arc::new(
                IndexUtxos::new(scope(), Network::Regtest, Arc::new(Outputs::Test))
                    .expect("fixture outputs"),
            ),
            rpc.clone(),
            rpc,
            Arc::new(EmptyHistory::Fixture),
        );

        let error = provider
            .create(SecretBytes::new([1_u8; 32]))
            .await
            .err()
            .expect("mismatched scope and native network must fail");

        assert_eq!(error.kind, WalletErrorKind::Unsupported);
    }
}
