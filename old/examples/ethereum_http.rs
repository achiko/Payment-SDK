use std::error::Error;

use payment_sdk::{
    Address, Ethereum, EthereumFiller, EthereumSigner, EthereumTransactionRequest, LocalSigner,
    NetworkWallet, ProviderBuilder, TransactionFiller, TransactionRequest, TxSigner,
};

fn create_transaction_request() -> EthereumTransactionRequest {
    let recipient = Address::<Ethereum>::new("0xreceiver");
    let value_wei = 1_000_u128;

    EthereumTransactionRequest::transfer(recipient, value_wei)
}

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Configure the signing and provider pipeline.
    // Generate a mock local credential, then adapt it to Ethereum.
    let credential = LocalSigner::generate()?;
    let signer = EthereumSigner::new(credential).with_chain_id(1);
    let signer_address = TxSigner::<Ethereum>::address(&signer).clone();

    // Passing one signer constructs an EthereumWallet through IntoWallet.
    let provider = ProviderBuilder::<Ethereum>::new()
        .wallet(signer)
        .connect_http("http://localhost:8545");

    assert_eq!(provider.wallet().default_signer_address(), &signer_address);

    // 2. Form the application-owned transaction request.
    let request = create_transaction_request();
    assert_eq!(request.from, None);
    assert_eq!(request.to.as_str(), "0xreceiver");
    assert_eq!(request.value_wei, 1_000);

    // 3. Add mock Ethereum network fields before signing.
    let filled_request = EthereumFiller.fill(request);
    assert_eq!(
        filled_request.steps(),
        [
            "ethereum transaction created",
            "nonce added",
            "ethereum fee added",
        ]
    );

    // 4. The provider adds the default sender, builds, signs, and sends.
    let result = provider.send_transaction(filled_request)?;

    assert_eq!(result.message, "ethereum transaction sent over HTTP");
    assert_eq!(result.tx_hash.as_str(), "0xmock-ethereum-hash");
    assert_eq!(result.rpc_message, "HTTP request sent");
    println!("{}", result.message);
    Ok(())
}
