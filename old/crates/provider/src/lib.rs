//! Typed provider construction, wallet integration, and mock broadcasting.

use std::{error::Error, fmt, marker::PhantomData};

use network::{
    Address, Chain, IntoWallet, NetworkWallet, SignedTxEnvelope, TransactionRequest, TxHash,
    WalletError,
};
use rpc_client::RpcClient;
use transport::Transport;

#[cfg(feature = "http")]
use transport::HttpTransport;
#[cfg(feature = "ws")]
use transport::WsTransport;

#[derive(Debug, Clone, Copy, Default)]
pub struct NoWallet;

#[derive(Debug, Clone)]
pub struct WalletFiller<W> {
    wallet: W,
}

impl<W> WalletFiller<W> {
    #[must_use]
    pub const fn new(wallet: W) -> Self {
        Self { wallet }
    }

    #[must_use]
    pub const fn wallet(&self) -> &W {
        &self.wallet
    }
}

impl<W> WalletFiller<W> {
    pub fn fill_and_sign<C>(
        &self,
        mut request: C::TransactionRequest,
    ) -> Result<C::SignedTxEnvelope, WalletError>
    where
        C: Chain,
        W: NetworkWallet<C>,
    {
        if request.sender().is_none() {
            request.set_sender(self.wallet.default_signer_address().clone());
        }
        self.wallet.sign_request(request)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderBuilder<C: Chain, F = NoWallet> {
    filler: F,
    chain: PhantomData<C>,
}

impl<C: Chain> ProviderBuilder<C, NoWallet> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            filler: NoWallet,
            chain: PhantomData,
        }
    }

    #[must_use]
    pub fn wallet<I>(self, input: I) -> ProviderBuilder<C, WalletFiller<I::Wallet>>
    where
        I: IntoWallet<C>,
    {
        ProviderBuilder {
            filler: WalletFiller::new(input.into_wallet()),
            chain: PhantomData,
        }
    }
}

impl<C: Chain> Default for ProviderBuilder<C, NoWallet> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Chain, F> ProviderBuilder<C, F> {
    #[must_use]
    pub fn connect<T: Transport>(self, transport: T) -> Provider<C, T, F> {
        Provider {
            client: RpcClient::new(transport),
            filler: self.filler,
            chain: PhantomData,
        }
    }

    #[cfg(feature = "http")]
    #[must_use]
    pub fn connect_http(self, endpoint: impl Into<String>) -> Provider<C, HttpTransport, F> {
        self.connect(HttpTransport::connect(endpoint))
    }

    #[cfg(feature = "ws")]
    pub async fn connect_ws(
        self,
        endpoint: impl Into<String>,
    ) -> Result<Provider<C, WsTransport, F>, ConnectError> {
        Ok(self.connect(WsTransport::connect(endpoint)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectError;

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("mock transport connection failed")
    }
}

impl Error for ConnectError {}

#[derive(Debug, Clone)]
pub struct Provider<C: Chain, T, F = NoWallet> {
    client: RpcClient<T>,
    filler: F,
    chain: PhantomData<C>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceResult {
    pub message: String,
    pub rpc_message: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission<C> {
    pub tx_hash: TxHash<C>,
    pub message: String,
    pub rpc_message: &'static str,
}

impl<C, T, F> Provider<C, T, F>
where
    C: Chain,
    T: Transport,
{
    #[must_use]
    pub fn endpoint(&self) -> &str {
        self.client.endpoint()
    }

    #[must_use]
    pub fn get_balance(&self, _address: &Address<C>) -> BalanceResult {
        BalanceResult {
            message: format!("{} balance requested over {}", C::NAME, T::NAME),
            rpc_message: self.client.get_balance(),
        }
    }

    #[must_use]
    pub fn send_envelope(&self, envelope: C::SignedTxEnvelope) -> Submission<C> {
        Submission {
            tx_hash: C::mock_tx_hash(),
            message: format!("{} transaction sent over {}", C::NAME, T::NAME),
            rpc_message: self.client.send_raw_transaction(envelope.message()),
        }
    }
}

impl<C, T, W> Provider<C, T, WalletFiller<W>>
where
    C: Chain,
    T: Transport,
    W: NetworkWallet<C>,
{
    pub fn send_transaction(
        &self,
        request: C::TransactionRequest,
    ) -> Result<Submission<C>, WalletError> {
        let envelope = self.filler.fill_and_sign(request)?;
        Ok(self.send_envelope(envelope))
    }

    #[must_use]
    pub const fn wallet(&self) -> &W {
        self.filler.wallet()
    }
}
