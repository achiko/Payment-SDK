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

pub(super) fn map_error(kind: WalletErrorKind, error: impl std::fmt::Display) -> WalletError {
    WalletError::new(kind, error.to_string())
}
