use std::sync::Arc;

use alloy_primitives::keccak256;
use base::{
    Address as BaseAddress, Addresser, Broadcaster, Decimal, KeyPair,
    Submission as BroadcastReceipt, TransactionBuilder as BaseBuilder, TransactionEnvelope,
    TransactionError, TransactionErrorKind, TransactionFuture, TransactionId, TransactionSnapshot,
};
use crypto::{PublicKeyFormat, SecretKey};
use indexing::{History as IndexHistory, IndexScope};
use wallets::{
    AddressEncoding, AddressFormat, AddressText, Balance, BalanceReader, Error as WalletError,
    ErrorKind as WalletErrorKind, FutureResult, Provider, SecretBytes, TransactionFactory,
    Wallet as WalletContract,
};

use crate::transaction::Preparation;
use crate::{
    Accounts, Address, AssetKind, ChainError, ChainErrorKind, SignedTransaction,
    TransactionCoordinator, TransferRequest, Wei,
};

const SNAPSHOT_KIND: &str = "ethereum.transfer";
const PREPARED_KIND: &str = "ethereum.signed";

mod history;
mod snapshot;

#[derive(Clone, Debug)]
pub struct WalletConfig {
    pub scope: IndexScope,
    /// The EVM chain this wallet signs for, verified against the node before
    /// broadcast.
    ///
    /// Configured rather than derived from the network slug: EVM chains are an
    /// open set, and a fixed slug table would reject every devnet, rollup, and
    /// fork while silently accepting a slug that names a different chain than
    /// the node actually serves.
    pub chain_id: u64,
    pub asset: AssetKind,
    pub decimals: u32,
}

impl WalletConfig {
    fn validate(&self) -> Result<(), WalletError> {
        if self.scope.chain.0 != "ethereum"
            || self.scope.network.trim().is_empty()
            || self.chain_id == 0
            || matches!(self.asset, AssetKind::Native) && self.decimals != crate::ETH.decimals
            || matches!(&self.asset, AssetKind::Erc20(token) if token.is_zero())
            || self.decimals > u8::MAX.into()
        {
            return Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "Ethereum wallet network, asset, and decimals must agree",
            ));
        }
        Ok(())
    }

    pub(crate) fn transfer_request(
        &self,
        from: Address,
        destination: Address,
        amount: &Decimal,
    ) -> Result<TransferRequest, TransactionError> {
        self.validate()
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        if amount <= &Decimal::zero() {
            return Err(transaction_error(
                TransactionErrorKind::InvalidAmount,
                "amount must be positive",
            ));
        }
        let value = amount
            .to_atomic_be_bytes(self.decimals)
            .map(Wei)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAmount, error))?;
        Ok(match &self.asset {
            AssetKind::Native => TransferRequest::native_atomic(from, destination, value),
            AssetKind::Erc20(token) => {
                TransferRequest::erc20(from, token.clone(), destination, value)
            }
        })
    }
}

pub struct WalletProvider {
    config: WalletConfig,
    accounts: Arc<dyn Accounts>,
    coordinator: Arc<TransactionCoordinator>,
    history: Arc<dyn IndexHistory>,
}

impl WalletProvider {
    #[must_use]
    pub fn new(
        config: WalletConfig,
        accounts: Arc<dyn Accounts>,
        coordinator: Arc<TransactionCoordinator>,
        history: Arc<dyn IndexHistory>,
    ) -> Self {
        Self {
            config,
            accounts,
            coordinator,
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
                let error = wallet_error(WalletErrorKind::Generation, error);
                Box::pin(async move { Err(error) })
            }
        }
    }

    #[must_use]
    pub fn transactions(&self) -> Arc<dyn wallets::Sender> {
        Arc::new(crate::batch::Batch::new(
            self.config.clone(),
            self.coordinator.clone(),
        ))
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
                coordinator: self.coordinator.clone(),
                history: self.history.clone(),
            }) as Arc<dyn WalletContract>)
        })
    }

    fn generate(&self) -> FutureResult<'_, Arc<dyn WalletContract>> {
        self.generate_with(SecretBytes::generate_secp256k1)
    }
}

struct Wallet {
    config: WalletConfig,
    address: Address,
    signer: Arc<KeyPair<Address>>,
    accounts: Arc<dyn Accounts>,
    coordinator: Arc<TransactionCoordinator>,
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

impl TransactionFactory for Wallet {
    fn transaction(&self) -> Box<dyn BaseBuilder> {
        Box::new(Builder::new(
            self.config.clone(),
            self.address.clone(),
            self.signer.clone(),
            self.coordinator.clone(),
        ))
    }

    fn restore(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn BaseBuilder>, TransactionError> {
        Ok(Box::new(Builder::restore(self, snapshot)?))
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

struct Builder {
    config: WalletConfig,
    from: Address,
    signer: Arc<KeyPair<Address>>,
    coordinator: Arc<TransactionCoordinator>,
    transfer: Option<(Address, Decimal)>,
}

impl Builder {
    fn restore(wallet: &Wallet, snapshot: &TransactionSnapshot) -> Result<Self, TransactionError> {
        snapshot::restore(wallet, snapshot)
    }

    fn new(
        config: WalletConfig,
        from: Address,
        signer: Arc<KeyPair<Address>>,
        coordinator: Arc<TransactionCoordinator>,
    ) -> Self {
        Self {
            config,
            from,
            signer,
            coordinator,
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
        self.config
            .transfer_request(self.from.clone(), destination, &amount)
    }

    fn validate(&self) -> Result<(), TransactionError> {
        self.config
            .validate()
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))
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
            .to_atomic_be_bytes::<32>(self.config.decimals)
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
                    "chain": self.config.scope.chain.0.as_str(),
                    "network": self.config.scope.network.as_str(),
                },
                "source": self.from.to_string(),
                "destination": destination.to_string(),
                "amount": amount.to_string(),
                "asset": asset_snapshot(&self.config.asset, self.config.decimals),
            }),
        ))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            let request = self.request()?;
            let signed = self
                .coordinator
                .prepare_one(Preparation::signer(
                    request,
                    self.config.chain_id,
                    self.signer.as_ref(),
                ))
                .await
                .map_err(preparation_error)?;
            Ok(base::SignedTransaction::new(
                PREPARED_KIND,
                TransactionId::new(signed.id.to_string()),
                TransactionEnvelope::new(signed.envelope),
            ))
        })
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
            let id = self.coordinator.broadcast(signed).await?;
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

pub(crate) fn preparation_error(error: ChainError) -> TransactionError {
    let kind = match error.kind {
        ChainErrorKind::InvalidAddress => TransactionErrorKind::InvalidAddress,
        ChainErrorKind::InvalidTransaction => TransactionErrorKind::InvalidTransaction,
        ChainErrorKind::InsufficientFunds => TransactionErrorKind::InsufficientFunds,
        ChainErrorKind::FeeUnavailable => TransactionErrorKind::Fee,
        ChainErrorKind::RpcUnavailable => TransactionErrorKind::Unavailable,
        ChainErrorKind::Divergent => TransactionErrorKind::Divergent,
        ChainErrorKind::Signer => TransactionErrorKind::Signing,
        ChainErrorKind::Rejected => TransactionErrorKind::Rejected,
        ChainErrorKind::NotFound => TransactionErrorKind::InvalidTransaction,
        ChainErrorKind::Other => TransactionErrorKind::Unknown,
    };
    transaction_error(kind, error)
}

fn wallet_error(kind: WalletErrorKind, error: impl std::fmt::Display) -> WalletError {
    WalletError::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransferIntent;
    use base::{Digest, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme};
    use futures_executor::block_on;
    use indexing::{ChainId, SourceError};

    struct InactiveDependencies;

    impl Accounts for InactiveDependencies {
        fn balance<'a>(
            &'a self,
            _address: Address,
            _asset: &'a AssetKind,
            _at: Option<indexing::BlockRef>,
        ) -> indexing::BoxFuture<'a, Result<Wei, SourceError>> {
            Box::pin(async { unreachable!("wallet generation must not read account balances") })
        }

        fn nonce<'a>(
            &'a self,
            _address: Address,
        ) -> indexing::BoxFuture<'a, Result<u64, SourceError>> {
            Box::pin(async { unreachable!("wallet generation must not read account nonces") })
        }
    }

    impl crate::Transactions for InactiveDependencies {
        fn build_context<'a>(
            &'a self,
            _request: &'a TransferRequest,
            _nonce: u64,
        ) -> indexing::BoxFuture<'a, Result<crate::BuildContext, ChainError>> {
            Box::pin(async { unreachable!("wallet generation must not build a transaction") })
        }

        fn broadcast<'a>(
            &'a self,
            _transaction: SignedTransaction,
        ) -> indexing::BoxFuture<'a, Result<crate::TransactionId, TransactionError>> {
            Box::pin(async { unreachable!("wallet generation must not broadcast a transaction") })
        }

        fn known<'a>(
            &'a self,
            _transaction: &'a crate::TransactionId,
        ) -> indexing::BoxFuture<'a, Result<bool, SourceError>> {
            Box::pin(async { unreachable!("wallet generation must not query a transaction") })
        }
    }

    impl IndexHistory for InactiveDependencies {
        fn history<'a>(
            &'a self,
            _request: indexing::HistoryQuery,
        ) -> indexing::BoxFuture<'a, Result<indexing::TransactionPage, indexing::IndexError>>
        {
            Box::pin(async { unreachable!("wallet generation must not read indexed history") })
        }
    }

    fn config(asset: AssetKind, decimals: u32) -> WalletConfig {
        WalletConfig {
            scope: IndexScope {
                chain: ChainId("ethereum".to_owned()),
                network: "sepolia".to_owned(),
            },
            chain_id: 11_155_111,
            asset,
            decimals,
        }
    }

    fn provider() -> WalletProvider {
        let dependencies = Arc::new(InactiveDependencies);
        let accounts: Arc<dyn Accounts> = dependencies.clone();
        let transactions: Arc<dyn crate::Transactions> = dependencies.clone();
        let history: Arc<dyn IndexHistory> = dependencies;
        let coordinator = Arc::new(TransactionCoordinator::new(accounts.clone(), transactions));
        WalletProvider::new(
            config(AssetKind::Native, crate::ETH.decimals),
            accounts,
            coordinator,
            history,
        )
    }

    #[test]
    fn generation_derives_address_from_native_secp256k1_signer() {
        let wallet = block_on(provider().generate())
            .expect("native Ethereum generation must create a wallet");
        let signed = block_on(wallet.sign(SignRequest {
            payload: SignablePayload::Digest(Digest {
                bytes: vec![0x42; 32],
            }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }))
        .expect("generated Ethereum wallet must sign with its native key");
        let hash = keccak256(&signed.public_key.bytes);
        let mut derived = [0_u8; 20];
        derived.copy_from_slice(&hash[12..]);
        let derived = Address(derived);
        let address = wallet.address();
        let text = wallet
            .address_text(&address)
            .expect("generated Ethereum address must format");

        assert_eq!(signed.public_key.curve, crypto::Curve::Secp256k1);
        assert_eq!(signed.public_key.format, PublicKeyFormat::Raw);
        assert_eq!(address, derived.address());
        assert_eq!(text.encoding, AddressEncoding::Hex);
        assert_eq!(text.text, derived.to_string());
    }

    #[test]
    fn generated_secret_reuses_create_and_preserves_signed_wire() {
        let provider = provider();
        let generated = block_on(provider.generate_with(fixed_secret))
            .expect("fixed valid secret must generate a wallet");
        let created = block_on(provider.create(SecretBytes::new([1_u8; 32])))
            .expect("fixed valid secret must create a wallet");
        let source = Address(
            generated
                .address()
                .as_bytes()
                .try_into()
                .expect("generated Ethereum address must contain 20 bytes"),
        );
        let transaction = crate::TransactionBuilder::new(
            TransferRequest::native_atomic(source, Address([0x22; 20]), Wei::from_u128(7)),
            crate::BuildContext {
                chain_id: 11_155_111,
                nonce: 2,
                gas_limit: 21_000,
                max_fee_per_gas: Wei::from_u128(10),
                max_priority_fee_per_gas: Wei::from_u128(3),
            },
        );
        let generated_signed = block_on(transaction.sign(generated.as_ref()))
            .expect("generated wallet must sign a valid EIP-1559 transaction");
        let created_signed = block_on(transaction.sign(created.as_ref()))
            .expect("created wallet must sign the same EIP-1559 transaction");
        let fees = generated_signed
            .inspect_eip1559_fees()
            .expect("generated signed wire must be a valid EIP-1559 envelope");

        assert_eq!(generated.address(), created.address());
        assert_eq!(generated_signed, created_signed);
        assert_eq!(generated_signed.envelope.first(), Some(&0x02));
        assert_eq!(
            generated_signed.id.0,
            keccak256(&generated_signed.envelope).0
        );
        assert_eq!(fees.chain_id, 11_155_111);
        assert_eq!(fees.gas_limit, 21_000);
    }

    #[test]
    fn generation_failure_is_typed_before_wallet_creation() {
        let mut provider = provider();
        provider.config.scope.chain = ChainId("not-ethereum".to_owned());

        let result = block_on(provider.generate_with(unavailable_secret));
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

    #[test]
    fn deterministic_preparation_kinds_remain_terminal_wallet_errors() {
        for (chain, expected) in [
            (
                ChainErrorKind::InsufficientFunds,
                TransactionErrorKind::InsufficientFunds,
            ),
            (ChainErrorKind::FeeUnavailable, TransactionErrorKind::Fee),
            (ChainErrorKind::Rejected, TransactionErrorKind::Rejected),
            (ChainErrorKind::Divergent, TransactionErrorKind::Divergent),
        ] {
            let mapped = preparation_error(ChainError {
                kind: chain,
                message: "terminal preparation failure".to_owned(),
            });
            assert_eq!(mapped.kind, expected);
        }
    }

    #[test]
    fn rpc_preparation_failures_remain_unavailable() {
        let mapped = preparation_error(ChainError {
            kind: ChainErrorKind::RpcUnavailable,
            message: "RPC failed".to_owned(),
        });
        assert_eq!(mapped.kind, TransactionErrorKind::Unavailable);
    }

    #[test]
    fn wallet_config_builds_native_and_token_requests_with_asset_precision() {
        let from = Address([0x11; 20]);
        let destination = Address([0x22; 20]);
        let native_amount = "0.000000000000000007"
            .parse::<Decimal>()
            .expect("native amount must parse");
        let token_amount = "0.000009"
            .parse::<Decimal>()
            .expect("token amount must parse");
        let token = Address([0x33; 20]);
        let seven_wei = Wei::from_u128(7);
        let nine_units = Wei::from_u128(9);

        let native = config(AssetKind::Native, 18)
            .transfer_request(from.clone(), destination.clone(), &native_amount)
            .expect("native request must use ETH precision");
        let erc20 = config(AssetKind::Erc20(token.clone()), 6)
            .transfer_request(from.clone(), destination.clone(), &token_amount)
            .expect("token request must use configured precision");

        assert_eq!(
            native.intent(),
            TransferIntent::Native {
                from: &from,
                to: &destination,
                value: &seven_wei,
            }
        );
        assert_eq!(
            erc20.intent(),
            TransferIntent::Erc20 {
                from: &from,
                token: &token,
                recipient: &destination,
                amount: &nine_units,
            }
        );
    }

    #[test]
    fn wallet_config_rejects_non_positive_transfer_amounts() {
        let error = config(AssetKind::Native, 18)
            .transfer_request(Address([0x11; 20]), Address([0x22; 20]), &Decimal::zero())
            .expect_err("zero-value transfers must fail before RPC");

        assert_eq!(error.kind, TransactionErrorKind::InvalidAmount);
    }
}
