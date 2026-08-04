use std::error::Error;

use payment_sdk::{
    Address, Bitcoin, BitcoinFiller, BitcoinNetwork, BitcoinSigner, BitcoinTransactionRequest,
    LocalSigner, NetworkWallet, ProviderBuilder, TransactionFiller, TransactionRequest, TxSigner,
};

fn create_transaction_request() -> BitcoinTransactionRequest {
    let recipient = Address::<Bitcoin>::new("bc1qreceiver");
    let amount_sats = 25_000_u64;

    BitcoinTransactionRequest::transfer(recipient, amount_sats)
}

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Configure the signing and provider pipeline.
    // Generate a mock local credential, then adapt it to Bitcoin mainnet.
    let credential = LocalSigner::generate()?;
    let signer = BitcoinSigner::new(credential, BitcoinNetwork::Mainnet);
    let signer_address = TxSigner::<Bitcoin>::address(&signer).clone();

    // Passing one signer constructs a BitcoinWallet through IntoWallet.
    let provider = ProviderBuilder::<Bitcoin>::new()
        .wallet(signer)
        .connect_http("http://localhost:8332");

    assert_eq!(provider.wallet().default_signer_address(), &signer_address);

    // 2. Form the application-owned transaction request.
    let request = create_transaction_request();
    assert_eq!(request.from, None);
    assert_eq!(request.to.as_str(), "bc1qreceiver");
    assert_eq!(request.amount_sats, 25_000);

    // 3. Add mock Bitcoin input-selection and fee fields before signing.
    let filled_request = BitcoinFiller.fill(request);
    assert_eq!(
        filled_request.steps(),
        [
            "bitcoin transaction created",
            "UTXO inputs selected",
            "bitcoin fee added",
        ]
    );

    // 4. The provider adds the default sender, builds, signs, and sends.
    let result = provider.send_transaction(filled_request)?;

    assert_eq!(result.message, "bitcoin transaction sent over HTTP");
    assert_eq!(result.tx_hash.as_str(), "mock-bitcoin-txid");
    assert_eq!(result.rpc_message, "HTTP request sent");
    println!("{}", result.message);
    Ok(())
}
