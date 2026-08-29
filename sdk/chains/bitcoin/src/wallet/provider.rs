use std::sync::Arc;

use base::{
    Address as BaseAddress, Addresser, Broadcaster, KeyPair, Submission as BroadcastReceipt,
    TransactionBuilder as BaseBuilder, TransactionError, TransactionErrorKind, TransactionFuture,
    TransactionId as BaseTransactionId,
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
    Address, FeeRate, Fees, IndexUtxos, Network, Satoshi, SignedTransaction, Transactions,
};

pub(super) const SNAPSHOT_KIND: &str = "bitcoin.transfer";
pub(crate) const PREPARED_KIND: &str = "bitcoin.signed";

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
    utxos: Arc<IndexUtxos>,
    fees: Arc<dyn Fees>,
    transactions: Arc<dyn Transactions>,
    history: Arc<dyn IndexHistory>,
}

impl Factory {
    #[must_use]
    pub fn new(
        config: Config,
        utxos: Arc<IndexUtxos>,
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

    fn generate_with(
        &self,
        generator: fn() -> Result<SecretBytes, crypto::Error>,
    ) -> FutureResult<'_, Arc<dyn WalletContract>> {
        match generator() {
            Ok(secret) => self.create(secret),
            Err(error) => {
                let error = map_error(WalletErrorKind::Generation, error);
                Box::pin(async move { Err(error) })
            }
        }
    }

    #[must_use]
    pub fn transactions(&self) -> Arc<dyn wallets::Sender> {
        Arc::new(crate::batch::Batch::new(
            self.config.network,
            self.utxos.clone(),
            self.fees.clone(),
            self.config.fee_target_blocks,
            self.config.max_fee_rate,
        ))
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

    fn generate(&self) -> FutureResult<'_, Arc<dyn WalletContract>> {
        self.generate_with(SecretBytes::generate_secp256k1)
    }
}

pub(super) struct Wallet {
    pub(super) config: Config,
    pub(super) address: Address,
    pub(super) signer: Arc<KeyPair<Address>>,
    pub(super) utxos: Arc<IndexUtxos>,
    pub(super) fees: Arc<dyn Fees>,
    pub(super) transactions: Arc<dyn Transactions>,
    pub(super) history: Arc<dyn IndexHistory>,
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

    fn restore(
        &self,
        snapshot: &base::TransactionSnapshot,
    ) -> Result<Box<dyn BaseBuilder>, TransactionError> {
        Ok(Box::new(super::builder::Builder::restore(self, snapshot)?))
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

pub(super) const fn network_name(network: Network) -> &'static str {
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
            let native_id = signed.id();
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
            let submitted = self
                .transactions
                .broadcast(signed, self.config.max_fee_rate)
                .await?;
            if submitted != native_id {
                return Err(transaction_error(
                    TransactionErrorKind::Unavailable,
                    "Bitcoin transaction capability returned a different transaction ID",
                ));
            }
            Ok(BroadcastReceipt {
                id: BaseTransactionId::new(native_id.to_string()),
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

pub(super) fn map_error(kind: WalletErrorKind, error: impl std::fmt::Display) -> WalletError {
    WalletError::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use base::{Digest, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme};
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction as NativeTransaction, TxIn, TxOut, Txid,
        Witness, absolute, consensus, hashes::Hash, transaction::Version,
    };
    use futures_executor::block_on;
    use indexing::{
        BoxFuture, ChainId, HistoryQuery, IndexError, OutputPage, OutputRequest, Outputs,
        SourceError, TransactionPage,
    };
    use std::sync::Mutex;

    use super::*;

    struct InactiveDependencies;

    impl Outputs for InactiveDependencies {
        fn list<'a>(
            &'a self,
            _request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async { unreachable!("wallet generation must not read indexed outputs") })
        }
    }

    impl Fees for InactiveDependencies {
        fn estimate<'a>(
            &'a self,
            _target_blocks: u16,
        ) -> BoxFuture<'a, Result<FeeRate, SourceError>> {
            Box::pin(async { unreachable!("wallet generation must not estimate fees") })
        }
    }

    impl Transactions for InactiveDependencies {
        fn preflight<'a>(
            &'a self,
            _transaction: &'a SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::Preflight, SourceError>> {
            Box::pin(async { unreachable!("wallet generation must not preflight a transaction") })
        }

        fn broadcast<'a>(
            &'a self,
            _transaction: SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::TransactionId, TransactionError>> {
            Box::pin(async { unreachable!("wallet generation must not broadcast a transaction") })
        }
    }

    impl IndexHistory for InactiveDependencies {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async { unreachable!("wallet generation must not read indexed history") })
        }
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct TransactionCalls {
        preflight: Vec<(crate::TransactionId, Vec<u8>)>,
        broadcast: Vec<(crate::TransactionId, Vec<u8>)>,
    }

    struct InspectingTransactions {
        preflight: Result<crate::Preflight, SourceError>,
        broadcast: Result<crate::TransactionId, TransactionError>,
        calls: Mutex<TransactionCalls>,
    }

    impl InspectingTransactions {
        fn new(
            preflight: Result<crate::Preflight, SourceError>,
            broadcast: Result<crate::TransactionId, TransactionError>,
        ) -> Self {
            Self {
                preflight,
                broadcast,
                calls: Mutex::new(TransactionCalls::default()),
            }
        }

        fn calls(&self) -> TransactionCalls {
            self.calls
                .lock()
                .expect("transaction call lock must be healthy")
                .clone()
        }
    }

    impl Transactions for InspectingTransactions {
        fn preflight<'a>(
            &'a self,
            transaction: &'a SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::Preflight, SourceError>> {
            self.calls
                .lock()
                .expect("transaction call lock must be healthy")
                .preflight
                .push((transaction.id(), transaction.consensus_bytes().to_vec()));
            let result = self.preflight.clone();
            Box::pin(async move { result })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: SignedTransaction,
            _max_fee_rate: FeeRate,
        ) -> BoxFuture<'a, Result<crate::TransactionId, TransactionError>> {
            self.calls
                .lock()
                .expect("transaction call lock must be healthy")
                .broadcast
                .push((transaction.id(), transaction.consensus_bytes().to_vec()));
            let result = self.broadcast.clone();
            Box::pin(async move { result })
        }
    }

    fn factory(address_type: AddressType) -> Factory {
        let transactions: Arc<dyn Transactions> = Arc::new(InactiveDependencies);
        factory_with_transactions(address_type, transactions)
    }

    fn factory_with_transactions(
        address_type: AddressType,
        transactions: Arc<dyn Transactions>,
    ) -> Factory {
        let network = Network::Regtest;
        let scope = IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: network_name(network).to_owned(),
        };
        let dependencies = Arc::new(InactiveDependencies);
        let outputs: Arc<dyn Outputs> = dependencies.clone();
        let fees: Arc<dyn Fees> = dependencies.clone();
        let history: Arc<dyn IndexHistory> = dependencies;
        let utxos = Arc::new(
            IndexUtxos::new(scope.clone(), network, outputs)
                .expect("fixture scope must match the Bitcoin network"),
        );
        Factory::new(
            Config {
                scope,
                network,
                address_type,
                fee_target_blocks: 6,
                max_fee_rate: FeeRate::new(1_000),
            },
            utxos,
            fees,
            transactions,
            history,
        )
    }

    fn prepared_transaction() -> (base::SignedTransaction, crate::TransactionId) {
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
        let id = crate::TransactionId::from(transaction.compute_txid());
        let envelope = consensus::serialize(&transaction);
        (
            base::SignedTransaction::new(
                PREPARED_KIND,
                BaseTransactionId::new(id.to_string()),
                base::TransactionEnvelope::new(envelope),
            ),
            id,
        )
    }

    fn allowed_preflight() -> crate::Preflight {
        crate::Preflight {
            allowed: true,
            reject_reason: None,
            virtual_size: None,
            base_fee: None,
        }
    }

    fn wallet_with_transactions(transactions: Arc<dyn Transactions>) -> Arc<dyn WalletContract> {
        block_on(
            factory_with_transactions(AddressType::SegwitV0, transactions)
                .create(SecretBytes::new([1_u8; 32])),
        )
        .expect("fixed valid secret must create a Bitcoin wallet")
    }

    #[test]
    fn broadcast_preserves_the_transaction_layers_exact_local_id() {
        let provider_candidate = format!("{:064x}", 9);
        let (prepared, native_id) = prepared_transaction();
        let local_id = BaseTransactionId::new(native_id.to_string());
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(allowed_preflight()),
            Err(transaction_error(
                TransactionErrorKind::Unavailable,
                format!("provider claimed transaction {provider_candidate}"),
            )
            .with_ambiguous_transaction_id(local_id.clone())),
        ));
        let wallet = wallet_with_transactions(transactions.clone());

        let error = block_on(wallet.broadcaster().broadcast(&prepared))
            .expect_err("an unknown Bitcoin broadcast outcome must fail with reconciliation data");

        assert_eq!(error.kind, TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, Some(local_id));
        assert_ne!(
            error
                .ambiguous_transaction_id
                .as_ref()
                .expect("ambiguity must carry the local ID")
                .as_str(),
            provider_candidate
        );
        let expected = (native_id, prepared.envelope().as_bytes().to_vec());
        let calls = transactions.calls();
        assert_eq!(calls.preflight, vec![expected.clone()]);
        assert_eq!(calls.broadcast, vec![expected]);
    }

    #[test]
    fn matching_broadcast_returns_the_validated_local_id() {
        let (prepared, native_id) = prepared_transaction();
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(allowed_preflight()),
            Ok(native_id),
        ));
        let wallet = wallet_with_transactions(transactions);

        let submitted = block_on(wallet.broadcaster().broadcast(&prepared))
            .expect("a matching Bitcoin broadcast ID must be acknowledged");

        assert_eq!(submitted.id, BaseTransactionId::new(native_id.to_string()));
    }

    #[test]
    fn invalid_local_envelope_has_no_ambiguity_or_chain_io() {
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(allowed_preflight()),
            Ok(crate::TransactionId([9; 32])),
        ));
        let wallet = wallet_with_transactions(transactions.clone());
        let (prepared, native_id) = prepared_transaction();
        let different_id = crate::TransactionId([9; 32]);
        assert_ne!(native_id, different_id);
        let invalid = base::SignedTransaction::new(
            PREPARED_KIND,
            BaseTransactionId::new(different_id.to_string()),
            base::TransactionEnvelope::new(prepared.envelope().as_bytes().to_vec()),
        );

        let error = block_on(wallet.broadcaster().broadcast(&invalid))
            .expect_err("a declared ID that disagrees with its bytes must fail locally");

        assert_eq!(error.kind, TransactionErrorKind::InvalidTransaction);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert_eq!(transactions.calls(), TransactionCalls::default());
    }

    #[test]
    fn preflight_failure_has_no_ambiguity_and_never_broadcasts() {
        let transactions = Arc::new(InspectingTransactions::new(
            Err(SourceError {
                message: "Bitcoin preflight is unavailable".to_owned(),
                retryable: true,
            }),
            Ok(crate::TransactionId([9; 32])),
        ));
        let wallet = wallet_with_transactions(transactions.clone());
        let (prepared, native_id) = prepared_transaction();

        let error = block_on(wallet.broadcaster().broadcast(&prepared))
            .expect_err("preflight failure must stop before broadcast");

        assert_eq!(error.kind, TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert_eq!(
            transactions.calls(),
            TransactionCalls {
                preflight: vec![(native_id, prepared.envelope().as_bytes().to_vec(),)],
                broadcast: Vec::new(),
            }
        );
    }

    #[test]
    fn rejected_preflight_has_no_ambiguity_and_never_broadcasts() {
        let transactions = Arc::new(InspectingTransactions::new(
            Ok(crate::Preflight {
                allowed: false,
                reject_reason: Some("missing-inputs".to_owned()),
                virtual_size: Some(82),
                base_fee: None,
            }),
            Ok(crate::TransactionId([9; 32])),
        ));
        let wallet = wallet_with_transactions(transactions.clone());
        let (prepared, _) = prepared_transaction();

        let error = block_on(wallet.broadcaster().broadcast(&prepared))
            .expect_err("a rejected Bitcoin preflight must stop before broadcast");

        assert_eq!(error.kind, TransactionErrorKind::Rejected);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert!(transactions.calls().broadcast.is_empty());
    }

    #[test]
    fn generation_builds_each_supported_regtest_address_type() {
        for (address_type, encoding, native_type, scheme, signature_encoding, public_format) in [
            (
                AddressType::SegwitV0,
                AddressEncoding::Bech32,
                bitcoin::AddressType::P2wpkh,
                SignatureScheme::EcdsaSecp256k1,
                SignatureEncoding::Der,
                PublicKeyFormat::Compressed,
            ),
            (
                AddressType::Taproot,
                AddressEncoding::Bech32m,
                bitcoin::AddressType::P2tr,
                SignatureScheme::SchnorrSecp256k1,
                SignatureEncoding::Raw,
                PublicKeyFormat::XOnly,
            ),
        ] {
            let wallet = block_on(factory(address_type).generate())
                .expect("native Bitcoin generation must create a wallet");
            let signed = block_on(wallet.sign(SignRequest {
                payload: SignablePayload::Digest(Digest {
                    bytes: vec![0x42; 32],
                }),
                scheme,
                encoding: signature_encoding,
                public_key_format: public_format,
                key_tweak: None,
            }))
            .expect("generated Bitcoin wallet must sign with its native key");
            let address = wallet
                .address_text(&wallet.address())
                .expect("generated address must use its configured Bitcoin format");
            let parsed = address
                .text
                .parse::<NativeAddress<bitcoin::address::NetworkUnchecked>>()
                .expect("generated address must be valid Bitcoin text")
                .require_network(bitcoin::Network::Regtest)
                .expect("generated address must use regtest");
            let derived = match address_type {
                AddressType::SegwitV0 => {
                    let key = PublicKey::from_slice(&signed.public_key.bytes)
                        .expect("signer must return a compressed Bitcoin public key");
                    let key = CompressedPublicKey::try_from(key)
                        .expect("signer public key must be compressed");
                    NativeAddress::p2wpkh(&key, bitcoin::Network::Regtest)
                }
                AddressType::Taproot => {
                    let key = XOnlyPublicKey::from_slice(&signed.public_key.bytes)
                        .expect("signer must return an x-only Bitcoin public key");
                    NativeAddress::p2tr(
                        &Secp256k1::verification_only(),
                        key,
                        None,
                        bitcoin::Network::Regtest,
                    )
                }
            };

            assert_eq!(address.encoding, encoding);
            assert_eq!(parsed.address_type(), Some(native_type));
            assert_eq!(signed.public_key.curve, crypto::Curve::Secp256k1);
            assert_eq!(signed.public_key.format, public_format);
            assert_eq!(address.text, derived.to_string());
        }
    }

    #[test]
    fn generated_secret_reuses_create_address_derivation() {
        for address_type in [AddressType::SegwitV0, AddressType::Taproot] {
            let factory = factory(address_type);
            let generated = block_on(factory.generate_with(fixed_secret))
                .expect("fixed valid secret must generate a wallet");
            let created = block_on(factory.create(SecretBytes::new([1_u8; 32])))
                .expect("fixed valid secret must create a wallet");

            assert_eq!(generated.address(), created.address());
        }
    }

    #[test]
    fn generation_failure_is_typed_before_wallet_creation() {
        let mut factory = factory(AddressType::SegwitV0);
        factory.config.scope.chain = ChainId("not-bitcoin".to_owned());

        let result = block_on(factory.generate_with(unavailable_secret));
        let error = match result {
            Ok(_) => panic!("generation failure must not create a wallet"),
            Err(error) => error,
        };

        assert_eq!(error.kind, WalletErrorKind::Generation);
        assert_eq!(
            error.message,
            "operating system random source is unavailable"
        );
    }

    fn unavailable_secret() -> Result<SecretBytes, crypto::Error> {
        Err(crypto::Error {
            kind: crypto::ErrorKind::KeyGeneration,
            message: "operating system random source is unavailable".to_owned(),
        })
    }

    fn fixed_secret() -> Result<SecretBytes, crypto::Error> {
        Ok(SecretBytes::new([1_u8; 32]))
    }
}
