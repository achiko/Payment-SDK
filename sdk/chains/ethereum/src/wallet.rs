use std::sync::Arc;

use alloy_primitives::keccak256;
use base::{
    Address as BaseAddress, Addresser, Broadcaster, BuilderCast, Decimal, KeyPair,
    Submission as BroadcastReceipt, TransactionBuilder as BaseBuilder, TransactionEnvelope,
    TransactionError, TransactionErrorKind, TransactionFuture, TransactionId, TransactionSnapshot,
};
use crypto::{PublicKeyFormat, SecretKey};
use indexing::{History as IndexHistory, IndexScope};
use wallets::{
    AddressEncoding, AddressFormat, AddressText, AmountFormat, Balance, BalanceReader,
    Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, Provider, SecretBytes,
    TransactionFactory, Wallet as WalletContract,
};

use crate::{
    Accounts, Address, AssetKind, SignedTransaction, TransactionBuilder, Transactions,
    TransferRequest, Wei,
};

const SNAPSHOT_KIND: &str = "ethereum.transfer.v1";
const PREPARED_KIND: &str = "ethereum.signed.v1";

mod history;
mod restore;
mod sweep;

#[derive(Clone, Debug)]
pub struct WalletConfig {
    pub scope: IndexScope,
    pub asset: AssetKind,
    pub decimals: u32,
}

impl WalletConfig {
    fn validate(&self) -> Result<(), WalletError> {
        if self.scope.chain.0 != "ethereum"
            || configured_chain_id(&self.scope).is_err()
            || matches!(self.asset, AssetKind::Native) && self.decimals != crate::ETH.decimals
            || self.decimals > u8::MAX.into()
        {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "Ethereum wallet network, asset, and decimals must agree",
            ));
        }
        Ok(())
    }
}

pub struct WalletProvider {
    config: WalletConfig,
    accounts: Arc<dyn Accounts>,
    transactions: Arc<dyn Transactions>,
    history: Arc<dyn IndexHistory>,
}

impl WalletProvider {
    #[must_use]
    pub fn new(
        config: WalletConfig,
        accounts: Arc<dyn Accounts>,
        transactions: Arc<dyn Transactions>,
        history: Arc<dyn IndexHistory>,
    ) -> Self {
        Self {
            config,
            accounts,
            transactions,
            history,
        }
    }
}

impl Provider for WalletProvider {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn WalletContract>> {
        Box::pin(async move {
            self.config.validate()?;
            let key = SecretKey::new(secret.as_bytes().to_vec())
                .map_err(|error| wallet_error(WalletErrorKind::InvalidSecret, error))?;
            let public = key
                .public_key(PublicKeyFormat::Raw)
                .map_err(|error| wallet_error(WalletErrorKind::InvalidSecret, error))?;
            let hash = keccak256(&public.bytes);
            let mut bytes = [0_u8; 20];
            bytes.copy_from_slice(&hash[12..]);
            let address = Address(bytes);
            let signer = KeyPair::new(address.clone(), secret.as_bytes().to_vec())
                .map_err(|error| wallet_error(WalletErrorKind::InvalidSecret, error))?;
            Ok(Arc::new(Wallet {
                config: self.config.clone(),
                address,
                signer: Arc::new(signer),
                accounts: self.accounts.clone(),
                transactions: self.transactions.clone(),
                history: self.history.clone(),
            }) as Arc<dyn WalletContract>)
        })
    }
}

struct Wallet {
    config: WalletConfig,
    address: Address,
    signer: Arc<KeyPair<Address>>,
    accounts: Arc<dyn Accounts>,
    transactions: Arc<dyn Transactions>,
    history: Arc<dyn IndexHistory>,
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

impl AddressFormat for Wallet {
    fn address_text(&self, address: &BaseAddress) -> Result<AddressText, WalletError> {
        let bytes: [u8; 20] = address.as_bytes().try_into().map_err(|_| {
            WalletError::new(
                WalletErrorKind::InvalidAddress,
                "Ethereum address must contain exactly 20 bytes",
            )
        })?;
        Ok(AddressText::new(
            AddressEncoding::Hex,
            Address(bytes).to_string(),
        ))
    }

    fn parse_address(&self, address: &AddressText) -> Result<BaseAddress, WalletError> {
        if address.encoding != AddressEncoding::Hex {
            return Err(WalletError::new(
                WalletErrorKind::InvalidAddress,
                "Ethereum addresses use hexadecimal encoding",
            ));
        }
        address
            .text
            .parse::<Address>()
            .map(|parsed| parsed.address())
            .map_err(|error| wallet_error(WalletErrorKind::InvalidAddress, error))
    }
}

impl BalanceReader for Wallet {
    fn balance<'a>(&'a self) -> FutureResult<'a, Balance> {
        Box::pin(async move {
            let amount = self
                .accounts
                .balance(self.address.clone(), &self.config.asset, None)
                .await
                .map_err(|error| wallet_error(WalletErrorKind::Balance, error))?;
            Ok(Balance {
                amount: Decimal::from_atomic(
                    num_bigint::BigUint::from_bytes_be(&amount.0),
                    self.config.decimals,
                ),
                observed_at: None,
            })
        })
    }
}

impl AmountFormat for Wallet {
    fn display_amount(&self, atomic: &Decimal) -> Result<Decimal, WalletError> {
        let units = atomic
            .to_atomic(0)
            .map_err(|error| wallet_error(WalletErrorKind::InvalidAmount, error))?;
        Ok(Decimal::from_atomic(units, self.config.decimals))
    }
}

impl TransactionFactory for Wallet {
    fn transaction(&self) -> Box<dyn BaseBuilder> {
        Box::new(Builder::new(
            self.config.scope.clone(),
            self.address.clone(),
            self.config.asset.clone(),
            self.config.decimals,
            self.signer.clone(),
            self.transactions.clone(),
        ))
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

struct Builder {
    scope: IndexScope,
    from: Address,
    asset: AssetKind,
    decimals: u32,
    signer: Arc<KeyPair<Address>>,
    transactions: Arc<dyn Transactions>,
    transfer: Option<(Address, Decimal)>,
}

impl Builder {
    fn new(
        scope: IndexScope,
        from: Address,
        asset: AssetKind,
        decimals: u32,
        signer: Arc<KeyPair<Address>>,
        transactions: Arc<dyn Transactions>,
    ) -> Self {
        Self {
            scope,
            from,
            asset,
            decimals,
            signer,
            transactions,
            transfer: None,
        }
    }

    fn request(&self) -> Result<TransferRequest, TransactionError> {
        self.validate()?;
        let (destination, amount) = self.transfer.clone().ok_or_else(|| {
            transaction_error(
                TransactionErrorKind::InvalidTransaction,
                "transfer is not configured",
            )
        })?;
        let value = amount
            .to_atomic_be_bytes(self.decimals)
            .map(Wei)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAmount, error))?;
        Ok(match &self.asset {
            AssetKind::Native => {
                TransferRequest::native_atomic(self.from.clone(), destination, value)
            }
            AssetKind::Erc20(token) => {
                TransferRequest::erc20(self.from.clone(), token.clone(), destination, value)
            }
        })
    }

    fn validate(&self) -> Result<(), TransactionError> {
        if self.scope.chain.0 != "ethereum"
            || configured_chain_id(&self.scope).is_err()
            || matches!(self.asset, AssetKind::Native) && self.decimals != crate::ETH.decimals
            || self.decimals > u8::MAX.into()
        {
            return Err(transaction_error(
                TransactionErrorKind::InvalidSnapshot,
                "Ethereum transaction identity, network, asset, and decimals do not agree",
            ));
        }
        Ok(())
    }
}

impl BuilderCast for Builder {
    fn utxo(&mut self) -> Option<&mut dyn base::UtxoBuilder> {
        None
    }
}

impl BaseBuilder for Builder {
    fn transfer(
        &mut self,
        destination: BaseAddress,
        amount: Decimal,
    ) -> Result<(), TransactionError> {
        if self.transfer.is_some() {
            return Err(transaction_error(
                TransactionErrorKind::Unsupported,
                "Ethereum transaction builder supports exactly one transfer",
            ));
        }
        let bytes: [u8; 20] = destination.as_bytes().try_into().map_err(|_| {
            transaction_error(
                TransactionErrorKind::InvalidAddress,
                "Ethereum destination must contain exactly 20 bytes",
            )
        })?;
        amount
            .to_atomic_be_bytes::<32>(self.decimals)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAmount, error))?;
        self.transfer = Some((Address(bytes), amount));
        Ok(())
    }

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        self.validate()?;
        let (destination, amount) = self.transfer.as_ref().ok_or_else(|| {
            transaction_error(
                TransactionErrorKind::InvalidTransaction,
                "transfer is not configured",
            )
        })?;
        Ok(TransactionSnapshot::new(
            SNAPSHOT_KIND,
            serde_json::json!({
                "scope": {
                    "chain": self.scope.chain.0.as_str(),
                    "network": self.scope.network.as_str(),
                },
                "source": self.from.to_string(),
                "destination": destination.to_string(),
                "amount": amount.to_string(),
                "asset": asset_snapshot(&self.asset, self.decimals),
            }),
        ))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            let request = self.request()?;
            let context = self
                .transactions
                .build_context(&request)
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            if context.chain_id != configured_chain_id(&self.scope)? {
                return Err(transaction_error(
                    TransactionErrorKind::Divergent,
                    "Ethereum RPC chain ID does not match the wallet network",
                ));
            }
            let signed = TransactionBuilder::new(request, context)
                .sign(self.signer.as_ref())
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Signing, error))?;
            Ok(base::SignedTransaction::new(
                PREPARED_KIND,
                TransactionId::new(signed.id.to_string()),
                TransactionEnvelope::new(signed.envelope),
            ))
        })
    }
}

fn configured_chain_id(scope: &IndexScope) -> Result<u64, TransactionError> {
    match scope.network.as_str() {
        "mainnet" => Ok(1),
        "sepolia" => Ok(11_155_111),
        _ => Err(transaction_error(
            TransactionErrorKind::InvalidSnapshot,
            "Ethereum wallet uses an unsupported network",
        )),
    }
}

fn asset_snapshot(asset: &AssetKind, decimals: u32) -> serde_json::Value {
    match asset {
        AssetKind::Native => serde_json::json!({
            "kind": "native",
            "ticker": crate::ETH.ticker,
            "decimals": decimals,
        }),
        AssetKind::Erc20(token) => serde_json::json!({
            "kind": "erc20",
            "token": token.to_string(),
            "decimals": decimals,
        }),
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
                    "prepared transaction is not an Ethereum signed envelope",
                ));
            }
            let id = prepared.id().as_str().parse().map_err(|error| {
                transaction_error(TransactionErrorKind::InvalidTransaction, error)
            })?;
            let signed =
                SignedTransaction::from_envelope(id, prepared.envelope().as_bytes().to_vec())
                    .map_err(|error| {
                        transaction_error(TransactionErrorKind::InvalidTransaction, error)
                    })?;
            let id = self
                .transactions
                .broadcast(signed)
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            Ok(BroadcastReceipt {
                id: TransactionId::new(id.to_string()),
            })
        })
    }
}

fn transaction_error(
    kind: TransactionErrorKind,
    error: impl std::fmt::Display,
) -> TransactionError {
    TransactionError::new(kind, error.to_string())
}

fn wallet_error(kind: WalletErrorKind, error: impl std::fmt::Display) -> WalletError {
    WalletError::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use indexing::{
        BoxFuture, ChainId, HistoryQuery, IndexError, ObservedTransaction, TransactionPage,
        TransactionQuery,
    };
    use wallets::{HistoryRequest, Wallets};

    use super::*;
    use crate::{Receipt, TransactionId};

    enum Rpc {
        Test,
    }

    enum LowBalance {
        Test,
    }

    impl Accounts for Rpc {
        fn balance<'a>(
            &'a self,
            _address: Address,
            _asset: &'a AssetKind,
            _at: Option<indexing::BlockRef>,
        ) -> crate::BoxFuture<'a, Result<Wei, indexing::SourceError>> {
            Box::pin(async { Ok(Wei::from_u128(1_500_000_000_000_000_000)) })
        }

        fn nonce<'a>(
            &'a self,
            _address: Address,
        ) -> crate::BoxFuture<'a, Result<u64, indexing::SourceError>> {
            Box::pin(async { Ok(7) })
        }
    }

    impl Transactions for Rpc {
        fn build_context<'a>(
            &'a self,
            _request: &'a TransferRequest,
        ) -> crate::BoxFuture<'a, Result<crate::BuildContext, indexing::SourceError>> {
            Box::pin(async {
                Ok(crate::BuildContext {
                    chain_id: 1,
                    nonce: 7,
                    gas_limit: 21_000,
                    max_fee_per_gas: Wei::from_u128(2_000_000_000),
                    max_priority_fee_per_gas: Wei::from_u128(1_000_000_000),
                })
            })
        }

        fn receipt<'a>(
            &'a self,
            id: &'a TransactionId,
        ) -> crate::BoxFuture<'a, Result<Option<Receipt>, indexing::SourceError>> {
            let id = id.clone();
            Box::pin(async move {
                Ok(Some(Receipt {
                    id,
                    included_in: None,
                    succeeded: Some(true),
                    confirmations: 2,
                }))
            })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: SignedTransaction,
        ) -> crate::BoxFuture<'a, Result<TransactionId, indexing::SourceError>> {
            Box::pin(async move { Ok(transaction.id) })
        }
    }

    impl Accounts for LowBalance {
        fn balance<'a>(
            &'a self,
            _address: Address,
            _asset: &'a AssetKind,
            _at: Option<indexing::BlockRef>,
        ) -> crate::BoxFuture<'a, Result<Wei, indexing::SourceError>> {
            Box::pin(async { Ok(Wei::from_u128(41_999_999_999_999)) })
        }

        fn nonce<'a>(
            &'a self,
            _address: Address,
        ) -> crate::BoxFuture<'a, Result<u64, indexing::SourceError>> {
            Box::pin(async { Ok(7) })
        }
    }

    impl Transactions for LowBalance {
        fn build_context<'a>(
            &'a self,
            _request: &'a TransferRequest,
        ) -> crate::BoxFuture<'a, Result<crate::BuildContext, indexing::SourceError>> {
            Box::pin(async {
                Ok(crate::BuildContext {
                    chain_id: 1,
                    nonce: 7,
                    gas_limit: 21_000,
                    max_fee_per_gas: Wei::from_u128(2_000_000_000),
                    max_priority_fee_per_gas: Wei::from_u128(1_000_000_000),
                })
            })
        }

        fn receipt<'a>(
            &'a self,
            _id: &'a TransactionId,
        ) -> crate::BoxFuture<'a, Result<Option<Receipt>, indexing::SourceError>> {
            Box::pin(async { Ok(None) })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: SignedTransaction,
        ) -> crate::BoxFuture<'a, Result<TransactionId, indexing::SourceError>> {
            Box::pin(async move { Ok(transaction.id) })
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
        "mainnet-native"
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "mainnet".to_owned(),
        }
    }

    #[tokio::test]
    async fn prepares_and_broadcasts_through_only_wallet_abstractions() {
        let rpc = Arc::new(Rpc::Test);
        let mut wallets = Wallets::new();
        wallets
            .register(
                kind(),
                WalletProvider::new(
                    WalletConfig {
                        scope: scope(),
                        asset: AssetKind::Native,
                        decimals: 18,
                    },
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
        assert_eq!(address.encoding, wallets::AddressEncoding::Hex);
        assert!(address.text.starts_with("0x"));
        assert_eq!(address.text.len(), 42);
        assert_eq!(
            wallet
                .parse_address(&address)
                .expect("canonical address text must round-trip"),
            wallet.address()
        );
        let atomic = num_bigint::BigUint::from(10_u8).pow(18);
        let display = wallet
            .display_amount(&Decimal::from_atomic(atomic.clone(), 0))
            .expect("atomic wei must convert exactly");
        assert_eq!(display.to_string(), "1");
        assert_eq!(display.to_atomic(18).expect("ETH must round-trip"), atomic);

        assert_eq!(
            wallet
                .balance()
                .await
                .expect("balance must load")
                .amount
                .to_string(),
            "1.5"
        );
        assert!(
            wallet
                .history(HistoryRequest::first(10))
                .await
                .expect("history must load")
                .transactions
                .is_empty()
        );

        let mut builder = wallet.transaction();
        builder
            .transfer(
                BaseAddress::from([2_u8; 20]),
                "0.25".parse::<Decimal>().expect("amount"),
            )
            .expect("transfer must configure");
        let snapshot = builder
            .snapshot()
            .expect("configured transfer must snapshot");
        assert!(snapshot.value().get("wallet").is_none());
        assert_eq!(snapshot.value()["scope"]["chain"], "ethereum");
        assert_eq!(snapshot.value()["scope"]["network"], "mainnet");
        assert_eq!(
            snapshot.value()["source"],
            Address(
                wallet
                    .address()
                    .as_bytes()
                    .try_into()
                    .expect("Ethereum wallet address has 20 bytes")
            )
            .to_string()
        );
        assert_eq!(snapshot.value()["asset"]["kind"], "native");
        assert_eq!(snapshot.value()["asset"]["decimals"], 18);
        let json = serde_json::to_vec(&snapshot).expect("snapshot must serialize");
        let decoded: TransactionSnapshot =
            serde_json::from_slice(&json).expect("snapshot must deserialize");
        let restored = wallet
            .restore(&decoded)
            .expect("this wallet must restore its snapshot");
        assert_eq!(restored.snapshot().expect("restored state"), snapshot);
        let prepared = builder.prepare().await.expect("transaction must sign");
        assert_eq!(prepared.id().as_str().len(), 66);
        let submission = wallet
            .broadcaster()
            .broadcast(&prepared)
            .await
            .expect("transaction must submit");
        assert_eq!(submission.id, prepared.id().clone());
    }

    #[tokio::test]
    async fn sweeps_native_balance_minus_the_maximum_fee() {
        let rpc = Arc::new(Rpc::Test);
        let mut wallets = Wallets::new();
        wallets
            .register(
                kind(),
                WalletProvider::new(
                    WalletConfig {
                        scope: scope(),
                        asset: AssetKind::Native,
                        decimals: 18,
                    },
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

        let prepared = wallet
            .sweep(BaseAddress::from([2_u8; 20]))
            .await
            .expect("balance above the fee ceiling must sweep");

        assert_eq!(
            prepared.fee,
            wallets::PreparedFee::Limit(Decimal::from(42_000_000_000_000_u64))
        );
        assert_eq!(prepared.transaction.kind(), PREPARED_KIND);
        assert_eq!(prepared.transaction.id().as_str().len(), 66);
    }

    #[tokio::test]
    async fn rejects_native_sweep_when_the_fee_consumes_the_balance() {
        let rpc = Arc::new(LowBalance::Test);
        let mut wallets = Wallets::new();
        wallets
            .register(
                kind(),
                WalletProvider::new(
                    WalletConfig {
                        scope: scope(),
                        asset: AssetKind::Native,
                        decimals: 18,
                    },
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

        let error = wallet
            .sweep(BaseAddress::from([2_u8; 20]))
            .await
            .expect_err("balance below the fee ceiling must fail closed");

        assert_eq!(error.kind, WalletErrorKind::InvalidAmount);
        assert!(error.message.contains("maximum sweep fee"));
    }

    #[tokio::test]
    async fn restore_rejects_another_wallet_network_or_asset() {
        let rpc = Arc::new(Rpc::Test);
        let mut wallets = Wallets::new();
        wallets
            .register(
                kind(),
                WalletProvider::new(
                    WalletConfig {
                        scope: scope(),
                        asset: AssetKind::Native,
                        decimals: 18,
                    },
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
                BaseAddress::from([2_u8; 20]),
                "0.25".parse::<Decimal>().expect("amount"),
            )
            .expect("transfer must configure");
        let snapshot = builder.snapshot().expect("transaction must snapshot");

        for (field, value) in [
            ("network", serde_json::json!("sepolia")),
            ("asset", serde_json::json!("USDC")),
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
    async fn repeated_transfer_is_rejected_without_replacing_the_first_transfer() {
        let rpc = Arc::new(Rpc::Test);
        let mut wallets = Wallets::new();
        wallets
            .register(
                kind(),
                WalletProvider::new(
                    WalletConfig {
                        scope: scope(),
                        asset: AssetKind::Native,
                        decimals: 18,
                    },
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
                BaseAddress::from([2_u8; 20]),
                "0.25".parse::<Decimal>().expect("amount"),
            )
            .expect("first transfer must configure");

        let error = builder
            .transfer(
                BaseAddress::from([3_u8; 20]),
                "0.5".parse::<Decimal>().expect("amount"),
            )
            .expect_err("second transfer must not silently replace the first");

        assert_eq!(error.kind, TransactionErrorKind::Unsupported);
        assert_eq!(
            builder.snapshot().expect("first transfer remains").value()["destination"],
            Address([2_u8; 20]).to_string()
        );
    }

    #[tokio::test]
    async fn token_snapshot_binds_contract_and_decimal_configuration() {
        let rpc = Arc::new(Rpc::Test);
        let token = Address([9_u8; 20]);
        let id = "mainnet-usdc";
        let mut wallets = Wallets::new();
        wallets
            .register(
                id,
                WalletProvider::new(
                    WalletConfig {
                        scope: scope(),
                        asset: AssetKind::Erc20(token.clone()),
                        decimals: 6,
                    },
                    rpc.clone(),
                    rpc,
                    Arc::new(EmptyHistory::Fixture),
                ),
            )
            .expect("wallet key must be unique");
        let wallet = wallets
            .new_wallet(&id, SecretBytes::new([1_u8; 32]))
            .await
            .expect("valid token configuration must create the wallet");
        let mut builder = wallet.transaction();
        builder
            .transfer(
                BaseAddress::from([2_u8; 20]),
                "12.5".parse::<Decimal>().expect("amount"),
            )
            .expect("token transfer must configure");

        let snapshot = builder.snapshot().expect("token transfer must snapshot");

        assert_eq!(snapshot.value()["asset"]["kind"], "erc20");
        assert_eq!(snapshot.value()["asset"]["token"], token.to_string());
        assert_eq!(snapshot.value()["asset"]["decimals"], 6);
    }

    #[tokio::test]
    async fn prepare_rejects_an_rpc_chain_that_disagrees_with_the_wallet_network() {
        let rpc = Arc::new(Rpc::Test);
        let mut wallets = Wallets::new();
        wallets
            .register(
                "sepolia-native",
                WalletProvider::new(
                    WalletConfig {
                        scope: IndexScope {
                            chain: ChainId(crate::CHAIN.to_owned()),
                            network: "sepolia".to_owned(),
                        },
                        asset: AssetKind::Native,
                        decimals: 18,
                    },
                    rpc.clone(),
                    rpc,
                    Arc::new(EmptyHistory::Fixture),
                ),
            )
            .expect("wallet key must be unique");
        let id = "sepolia-native";
        let wallet = wallets
            .new_wallet(&id, SecretBytes::new([1_u8; 32]))
            .await
            .expect("valid sepolia configuration must create the wallet");
        let mut builder = wallet.transaction();
        builder
            .transfer(
                BaseAddress::from([2_u8; 20]),
                "0.25".parse::<Decimal>().expect("amount"),
            )
            .expect("transfer must configure");

        let error = builder
            .prepare()
            .await
            .expect_err("mainnet RPC context must not prepare a sepolia transaction");

        assert_eq!(error.kind, TransactionErrorKind::Divergent);
    }
}
