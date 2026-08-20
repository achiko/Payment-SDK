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

use crate::{
    Accounts, Address, AssetKind, SignedTransaction, TransactionBuilder, Transactions,
    TransferRequest, Wei,
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

    #[must_use]
    pub fn transactions(&self) -> Arc<dyn wallets::Sender> {
        Arc::new(crate::batch::Batch::Sequential)
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

impl TransactionFactory for Wallet {
    fn transaction(&self) -> Box<dyn BaseBuilder> {
        Box::new(Builder::new(
            self.config.scope.clone(),
            self.config.chain_id,
            self.address.clone(),
            self.config.asset.clone(),
            self.config.decimals,
            self.signer.clone(),
            self.transactions.clone(),
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
    scope: IndexScope,
    chain_id: u64,
    from: Address,
    asset: AssetKind,
    decimals: u32,
    signer: Arc<KeyPair<Address>>,
    transactions: Arc<dyn Transactions>,
    transfer: Option<(Address, Decimal)>,
}

impl Builder {
    fn restore(wallet: &Wallet, snapshot: &TransactionSnapshot) -> Result<Self, TransactionError> {
        snapshot::restore(wallet, snapshot)
    }

    fn new(
        scope: IndexScope,
        chain_id: u64,
        from: Address,
        asset: AssetKind,
        decimals: u32,
        signer: Arc<KeyPair<Address>>,
        transactions: Arc<dyn Transactions>,
    ) -> Self {
        Self {
            scope,
            chain_id,
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
            || self.scope.network.trim().is_empty()
            || self.chain_id == 0
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
            if context.chain_id != self.chain_id {
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
