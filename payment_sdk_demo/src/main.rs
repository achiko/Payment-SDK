use chain_ethereum::{
    EthereumHttpRpc, EthereumHttpRpcConfig, EthereumRpcLimits, EthereumTransferRequest, Wei,
};
use payment_http::RetryPolicy;
use signer::{Curve, KeyProvisionRequest, KeyProvisioner, OperationId, PublicKeyFormat};
use signer_local::LocalSigner;
use std::time::Duration;
use std::{error::Error, sync::Arc};
use wallet_worker::WalletService;

use chain_ethereum::{
    Ethereum, EthereumAddress, EthereumAsset, EthereumGenerateAddress, EthereumWallet,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let limits = EthereumRpcLimits::new(
        128 * 1024,                                // Maximum transaction input
        2_000,                                     // 20% gas margin
        30_000_000,                                // Maximum gas limit
        Wei::from_u128(500_000_000_000),           // Maximum fee per gas
        Wei::from_u128(50_000_000_000),            // Maximum priority fee
        Wei::from_u128(1_000_000_000_000_000_000), // Maximum total fee
    )?;

    let rpc_config = EthereumHttpRpcConfig::new(
        "http://127.0.0.1:8545",
        31_337,
        Duration::from_secs(5),
        1024 * 1024,
        RetryPolicy::no_retry(),
        limits,
    )?;

    let rpc = EthereumHttpRpc::new(rpc_config)?;
    rpc.verify_chain_id().await?;
    println!("Connected to Ethereum chain 31337");

    let signer = Arc::new(LocalSigner::ephemeral_for_testing());
    // println!("In-memory signer ready: {signer:?}");

    let _key = signer
        .provision(KeyProvisionRequest {
            operation_id: OperationId::new("generate-key-1")?,
            curve: Curve::Secp256k1,
            public_key_format: PublicKeyFormat::Raw,
            purpose: "test-key".to_owned(),
        })
        .await?;

    let wallet = EthereumWallet::new(31_337, rpc);

    let service = WalletService::<Ethereum, _, _, _>::new(wallet, Arc::clone(&signer), signer);

    let generated = service
        .generate_address(
            &EthereumAsset::Native,
            EthereumGenerateAddress::new(
                31_337,
                OperationId::new("generate-eth-address-1")?,
                "wallet-integration",
            ),
        )
        .await?;

    println!("Ethereum address: {}", generated.address);
    // println!("Public key: {:?}", generated.public_key);
    // println!("Key locator: {:?}", generated.key);

    println!("Fund the address, then press Ctrl+C to check its balance.");
    // tokio::signal::ctrl_c().await?;

    let _balance = service
        .balance(&EthereumAsset::Native, &generated.address)
        .await?;

    // println!("Balance: {balance:#?}");
    let recipient = "0x1234567890123456789012345678901234567890".parse::<EthereumAddress>()?;

    // std::env::var("RECIPIENT_ADDRESS")?.parse()?;

    let request = EthereumTransferRequest::native(
        OperationId::new("send-eth-1")?,
        generated.key.clone(),
        generated.address.clone(),
        recipient,
        Wei::from_u128(100_000_000_000_000_000), // 0.1 ETH
    );

    let unsigned_tx = service
        .build_transfer(&EthereumAsset::Native, request)
        .await?;

    let signed_tx = service
        .sign_transaction(&EthereumAsset::Native, unsigned_tx)
        .await?;

    println!("Signed transaction ID: {:?}", signed_tx.id);

    let transaction_id = service.broadcast(&EthereumAsset::Native, signed_tx).await?;

    println!("Broadcast transaction ID: {transaction_id}");

    let receipt = service
        .transaction(&EthereumAsset::Native, &transaction_id)
        .await?;

    println!("Receipt: {receipt:#?}");

    Ok(())
}
