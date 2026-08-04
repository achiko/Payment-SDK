use std::error::Error;

use payment_sdk::{
    Address, Bitcoin, BitcoinFiller, BitcoinNetwork, BitcoinSigner, BitcoinTransactionRequest,
    Ethereum, EthereumFiller, EthereumSigner, EthereumTransactionRequest, EthereumWallet,
    LocalSigner, NetworkWallet, ProviderBuilder, TransactionFiller, TransactionRequest, TxSigner,
    WalletError, WalletFiller,
};

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn ethereum_http_pipeline_builds_signs_and_sends() -> TestResult {
    let signer = EthereumSigner::new(LocalSigner::generate()?).with_chain_id(1);
    let provider = ProviderBuilder::<Ethereum>::new()
        .wallet(signer)
        .connect_http("http://localhost:8545");
    let request = EthereumFiller.fill(EthereumTransactionRequest::new());

    assert_eq!(
        request.steps(),
        [
            "ethereum transaction created",
            "nonce added",
            "ethereum fee added",
        ]
    );

    let result = provider.send_transaction(request)?;
    assert_eq!(result.message, "ethereum transaction sent over HTTP");
    assert_eq!(result.tx_hash.as_str(), "0xmock-ethereum-hash");
    assert_eq!(result.rpc_message, "HTTP request sent");
    Ok(())
}

#[test]
fn bitcoin_http_pipeline_builds_signs_and_sends() -> TestResult {
    let signer = BitcoinSigner::new(LocalSigner::generate()?, BitcoinNetwork::Mainnet);
    let provider = ProviderBuilder::<Bitcoin>::new()
        .wallet(signer)
        .connect_http("http://localhost:8332");
    let request = BitcoinFiller.fill(BitcoinTransactionRequest::new());

    assert_eq!(
        request.steps(),
        [
            "bitcoin transaction created",
            "UTXO inputs selected",
            "bitcoin fee added",
        ]
    );

    let result = provider.send_transaction(request)?;
    assert_eq!(result.message, "bitcoin transaction sent over HTTP");
    assert_eq!(result.tx_hash.as_str(), "mock-bitcoin-txid");
    assert_eq!(result.rpc_message, "HTTP request sent");
    Ok(())
}

#[test]
fn ethereum_wallet_routes_by_request_sender() -> TestResult {
    let default = EthereumSigner::new(LocalSigner::generate()?);
    let selected = EthereumSigner::new(LocalSigner::generate()?);
    let selected_address = TxSigner::<Ethereum>::address(&selected).clone();
    let mut wallet = EthereumWallet::new(default);
    wallet.register_signer(selected);

    let provider = ProviderBuilder::<Ethereum>::new()
        .wallet(wallet)
        .connect_http("http://localhost:8545");
    let request = EthereumTransactionRequest::new().from(selected_address.clone());
    let result = provider.send_transaction(request)?;

    assert!(provider.wallet().has_signer_for(&selected_address));
    assert_eq!(provider.wallet().signer_addresses().len(), 2);
    assert_eq!(result.message, "ethereum transaction sent over HTTP");
    Ok(())
}

#[test]
fn wallet_rejects_an_unregistered_sender() -> TestResult {
    let signer = EthereumSigner::new(LocalSigner::generate()?);
    let provider = ProviderBuilder::<Ethereum>::new()
        .wallet(signer)
        .connect_http("http://localhost:8545");
    let request = EthereumTransactionRequest::new().from(Address::new("0xmissing"));

    assert_eq!(
        provider.send_transaction(request).err(),
        Some(WalletError::SignerNotFound {
            address: "0xmissing".to_owned(),
        })
    );
    Ok(())
}

#[test]
fn provider_wallet_filler_adds_the_default_sender() -> TestResult {
    let signer = EthereumSigner::new(LocalSigner::generate()?);
    let default_address = TxSigner::<Ethereum>::address(&signer).clone();
    let wallet = EthereumWallet::new(signer);
    let filler = WalletFiller::new(wallet);

    let signed = filler.fill_and_sign::<Ethereum>(EthereumTransactionRequest::new())?;

    assert_eq!(
        signed.transaction().request().from.as_ref(),
        Some(&default_address)
    );
    Ok(())
}

#[cfg(feature = "ws")]
#[test]
fn ethereum_websocket_provider_uses_the_same_read_api() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    use payment_sdk::ConnectError;

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(future);

        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("mock connection future unexpectedly returned Pending"),
        }
    }

    fn connect() -> Result<(), ConnectError> {
        let provider =
            block_on_ready(ProviderBuilder::<Ethereum>::new().connect_ws("ws://localhost:8546"))?;
        let result = provider.get_balance(&Address::new("0xaccount"));

        assert_eq!(result.message, "ethereum balance requested over WebSocket");
        assert_eq!(result.rpc_message, "WebSocket request sent");
        Ok(())
    }

    assert_eq!(connect(), Ok(()));
}
