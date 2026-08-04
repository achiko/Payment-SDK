//! Runnable composition example for Bitcoin, native ETH, and Ethereum-mainnet USDC.
//!
//! Bitcoin and Ethereum deliberately remain separate typed services. ETH and
//! USDC share one Ethereum service because USDC is an ERC-20 asset on Ethereum.
//! The RPC implementations below are deterministic offline demonstrations.
//! Replace them, and `LocalSigner`, with authenticated production adapters.

use chain_bitcoin::{
    Bitcoin, BitcoinAddressKind, BitcoinAsset, BitcoinBatchCollectionRequest, BitcoinBlock,
    BitcoinCollectionSource, BitcoinGenerateAddress, BitcoinNetwork, BitcoinReceipt, BitcoinRpc,
    BitcoinRpcUtxo, BitcoinSignedTransaction, BitcoinTransactionId, BitcoinWallet,
    BoxFuture as BitcoinFuture, Satoshi,
};
use chain_ethereum::{
    BoxFuture as EthereumFuture, Ethereum, EthereumAddress, EthereumAsset, EthereumBuildContext,
    EthereumCollectionRequest, EthereumGenerateAddress, EthereumReceipt, EthereumRpc,
    EthereumSignedTransaction, EthereumTransactionId, EthereumTransferRequest, EthereumWallet, Wei,
};
use futures_executor::block_on;
use indexing::{BlockHeight, BlockRef, SourceError};
use signer::{KeyProvisioner, Signer};
use signer_local::LocalSigner;
use std::{error::Error, sync::Arc};
use transaction_utxo::FeeRate;
use wallet_worker::WalletService;

const BITCOIN_NETWORK: BitcoinNetwork = BitcoinNetwork::Testnet4;
const ETHEREUM_CHAIN_ID: u64 = 1;

/// Ethereum-mainnet USDC (`6` decimal places).
/// Source: <https://developers.circle.com/stablecoins/usdc-contract-addresses>
const USDC_CONTRACT: EthereumAddress = EthereumAddress([
    0xa0, 0xb8, 0x69, 0x91, 0xc6, 0x21, 0x8b, 0x36, 0xc1, 0xd1, 0x9d, 0x4a, 0x2e, 0x9e, 0xb0, 0xce,
    0x36, 0x06, 0xeb, 0x48,
]);

/// Application composition for three supported assets without erasing their
/// chain-native transaction types.
#[derive(Debug)]
struct ThreeAssetWalletService<BR, ER, K, S> {
    bitcoin: WalletService<Bitcoin, BitcoinWallet<BR>, K, S>,
    ethereum: WalletService<Ethereum, EthereumWallet<ER>, K, S>,
    usdc: EthereumAsset,
}

impl<BR, ER, K, S> ThreeAssetWalletService<BR, ER, K, S>
where
    BR: BitcoinRpc,
    ER: EthereumRpc,
    K: KeyProvisioner + Clone,
    S: Signer + Clone,
{
    fn new(bitcoin_rpc: BR, ethereum_rpc: ER, keys: K, signer: S) -> Self {
        Self {
            bitcoin: WalletService::new(
                BitcoinWallet::new(BITCOIN_NETWORK, bitcoin_rpc),
                keys.clone(),
                signer.clone(),
            ),
            ethereum: WalletService::new(
                EthereumWallet::new(ETHEREUM_CHAIN_ID, ethereum_rpc),
                keys,
                signer,
            ),
            usdc: EthereumAsset::Erc20(USDC_CONTRACT),
        }
    }

    fn bitcoin(&self) -> &WalletService<Bitcoin, BitcoinWallet<BR>, K, S> {
        &self.bitcoin
    }

    fn ethereum(&self) -> &WalletService<Ethereum, EthereumWallet<ER>, K, S> {
        &self.ethereum
    }

    fn usdc(&self) -> &EthereumAsset {
        &self.usdc
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    block_on(run())
}

async fn run() -> Result<(), Box<dyn Error>> {
    println!("OFFLINE DEMO ONLY — the generated keys and RPC values are ephemeral test data.\n");

    // One custody backend can provision and sign for both chains because the
    // Wallet Service stores only opaque key locators, never private keys.
    let custody = Arc::new(LocalSigner::ephemeral_for_testing());
    let service = ThreeAssetWalletService::new(
        DemoBitcoinRpc,
        DemoEthereumRpc,
        Arc::clone(&custody),
        custody,
    );

    let bitcoin_asset = BitcoinAsset::Native;
    let ethereum_asset = EthereumAsset::Native;
    let usdc_asset = service.usdc().clone();

    let bitcoin_deposit = service
        .bitcoin()
        .generate_address(
            &bitcoin_asset,
            BitcoinGenerateAddress::new(
                BITCOIN_NETWORK,
                BitcoinAddressKind::SegwitV0,
                "deposit:btc:customer-42",
            ),
        )
        .await?;
    let bitcoin_treasury = service
        .bitcoin()
        .generate_address(
            &bitcoin_asset,
            BitcoinGenerateAddress::new(
                BITCOIN_NETWORK,
                BitcoinAddressKind::Taproot,
                "treasury:btc",
            ),
        )
        .await?;
    let ethereum_deposit = service
        .ethereum()
        .generate_address(
            &ethereum_asset,
            EthereumGenerateAddress::new(ETHEREUM_CHAIN_ID, "deposit:eth:customer-42"),
        )
        .await?;
    let usdc_deposit = service
        .ethereum()
        .generate_address(
            &usdc_asset,
            EthereumGenerateAddress::new(ETHEREUM_CHAIN_ID, "deposit:usdc:customer-42"),
        )
        .await?;
    let ethereum_treasury = service
        .ethereum()
        .generate_address(
            &ethereum_asset,
            EthereumGenerateAddress::new(ETHEREUM_CHAIN_ID, "treasury:ethereum"),
        )
        .await?;

    println!("Configured assets");
    println!("BTC  deposit: {}", bitcoin_deposit.address.0);
    println!(
        "ETH  deposit: 0x{}",
        encode_hex(&ethereum_deposit.address.0)
    );
    println!("USDC contract: 0x{}", encode_hex(&USDC_CONTRACT.0));
    println!("USDC deposit: 0x{}", encode_hex(&usdc_deposit.address.0));

    let bitcoin_balance = service
        .bitcoin()
        .balance(&bitcoin_asset, &bitcoin_deposit.address)
        .await?;
    let ethereum_balance = service
        .ethereum()
        .balance(&ethereum_asset, &ethereum_deposit.address)
        .await?;
    let usdc_balance = service
        .ethereum()
        .balance(&usdc_asset, &usdc_deposit.address)
        .await?;

    println!("\nBalances returned by the demo RPCs");
    println!("BTC:  {} satoshis", bitcoin_balance.spendable.0);
    println!("ETH:  {} wei", display_wei(&ethereum_balance.spendable));
    println!("USDC: {}", display_usdc(&usdc_balance.spendable));

    // Bitcoin collection discovers confirmed UTXOs, builds a drain transaction,
    // signs every input, broadcasts once, and attributes the gross source value.
    let bitcoin_collection = BitcoinBatchCollectionRequest {
        sources: vec![BitcoinCollectionSource {
            address: bitcoin_deposit.address,
            key: bitcoin_deposit.key,
            birthday: BlockHeight(0),
        }],
        destination: bitcoin_treasury.address,
        minimum_confirmations: 1,
        fee_rate: None,
    };
    let bitcoin_requirements = service
        .bitcoin()
        .collection_requirements(&bitcoin_asset, &bitcoin_collection)
        .await?;
    let bitcoin_submission = service
        .bitcoin()
        .collect(&bitcoin_asset, bitcoin_collection)
        .await?;

    println!("\nBitcoin collection");
    println!("requirements: {bitcoin_requirements:?}");
    println!(
        "transaction ID bytes: {}",
        encode_hex(&bitcoin_submission.transaction_id.0)
    );
    for attribution in bitcoin_submission.attribution {
        println!(
            "gross input attributed to {}: {} satoshis",
            attribution.address.0, attribution.gross_input.0
        );
    }

    // Native ETH demonstrates the explicit build -> sign -> broadcast lifecycle.
    let unsigned_ethereum = service
        .ethereum()
        .build_transfer(
            &ethereum_asset,
            EthereumTransferRequest {
                key: ethereum_deposit.key,
                from: ethereum_deposit.address,
                to: Some(ethereum_treasury.address.clone()),
                value: Wei::from_u128(1_000_000_000_000_000),
                data: Vec::new(),
            },
        )
        .await?;
    let signed_ethereum = service
        .ethereum()
        .sign_transaction(&ethereum_asset, unsigned_ethereum)
        .await?;
    let ethereum_transaction_id = service
        .ethereum()
        .broadcast(&ethereum_asset, signed_ethereum)
        .await?;

    println!("\nEthereum transfer");
    println!(
        "transaction hash: 0x{}",
        encode_hex(&ethereum_transaction_id.0)
    );

    // USDC is routed through the same Ethereum service. Requirements first
    // report whether the token deposit needs native ETH for gas; Payment Service
    // would fund any reported deficit before retrying this collection.
    let usdc_collection = EthereumCollectionRequest::Token {
        token: USDC_CONTRACT,
        from: usdc_deposit.address,
        key: usdc_deposit.key,
        destination: ethereum_treasury.address,
        amount: None,
    };
    let usdc_requirements = service
        .ethereum()
        .collection_requirements(&usdc_asset, &usdc_collection)
        .await?;
    let usdc_submission = service
        .ethereum()
        .collect(&usdc_asset, usdc_collection)
        .await?;

    println!("\nUSDC collection");
    println!("requirements: {usdc_requirements:?}");
    println!(
        "transaction hash: 0x{}",
        encode_hex(&usdc_submission.transaction_id.0)
    );
    for attribution in usdc_submission.attribution {
        println!(
            "gross USDC debit attributed to 0x{}: {}",
            encode_hex(&attribution.address.0),
            display_usdc(&attribution.gross_debit)
        );
    }
    println!("\nNo private key, credential, or signed transaction bytes were printed.");

    Ok(())
}

/// Offline Bitcoin data source. A real implementation would call Bitcoin Core,
/// Esplora, Electrum, or another indexed backend and enforce timeouts/auth.
#[derive(Clone, Copy, Debug)]
struct DemoBitcoinRpc;

impl BitcoinRpc for DemoBitcoinRpc {
    fn tip<'a>(&'a self) -> BitcoinFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async { Err(offline_error("Bitcoin tip")) })
    }

    fn block_at<'a>(
        &'a self,
        _height: BlockHeight,
    ) -> BitcoinFuture<'a, Result<BitcoinBlock, SourceError>> {
        Box::pin(async { Err(offline_error("Bitcoin block lookup")) })
    }

    fn utxos<'a>(
        &'a self,
        scripts: Vec<Vec<u8>>,
    ) -> BitcoinFuture<'a, Result<Vec<BitcoinRpcUtxo>, SourceError>> {
        Box::pin(async move {
            Ok(scripts
                .into_iter()
                .enumerate()
                .map(|(index, script_pubkey)| {
                    let marker =
                        u8::try_from(index).map_or(u8::MAX, |value| value.saturating_add(1));
                    BitcoinRpcUtxo {
                        transaction_id: [marker; 32],
                        output_index: 0,
                        value: Satoshi(150_000),
                        script_pubkey,
                        confirmations: 6,
                        coinbase: false,
                    }
                })
                .collect())
        })
    }

    fn estimate_fee_rate<'a>(&'a self) -> BitcoinFuture<'a, Result<FeeRate, SourceError>> {
        Box::pin(async {
            Ok(FeeRate {
                units_per_weight: 1,
            })
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
    ) -> BitcoinFuture<'a, Result<BitcoinTransactionId, SourceError>> {
        Box::pin(async move { Ok(transaction.id) })
    }

    fn receipt<'a>(
        &'a self,
        _id: &'a BitcoinTransactionId,
    ) -> BitcoinFuture<'a, Result<Option<BitcoinReceipt>, SourceError>> {
        Box::pin(async { Ok(None) })
    }
}

/// Offline Ethereum data source. Replace it with an authenticated Ethereum
/// JSON-RPC implementation that supplies real balances, nonces, and gas data.
#[derive(Clone, Copy, Debug)]
struct DemoEthereumRpc;

impl EthereumRpc for DemoEthereumRpc {
    fn balance<'a>(
        &'a self,
        _address: EthereumAddress,
        asset: &'a EthereumAsset,
        _at: Option<BlockRef>,
    ) -> EthereumFuture<'a, Result<Wei, SourceError>> {
        let balance = match asset {
            EthereumAsset::Native => Wei::from_u128(10_000_000_000_000_000),
            EthereumAsset::Erc20(contract) if contract == &USDC_CONTRACT => {
                Wei::from_u128(25_000_000)
            }
            EthereumAsset::Erc20(_) => Wei::ZERO,
        };
        Box::pin(async move { Ok(balance) })
    }

    fn nonce<'a>(
        &'a self,
        _address: EthereumAddress,
    ) -> EthereumFuture<'a, Result<u64, SourceError>> {
        Box::pin(async { Ok(7) })
    }

    fn build_context<'a>(
        &'a self,
        request: &'a EthereumTransferRequest,
    ) -> EthereumFuture<'a, Result<EthereumBuildContext, SourceError>> {
        let gas_limit = if request.data.is_empty() {
            21_000
        } else {
            65_000
        };
        Box::pin(async move {
            Ok(EthereumBuildContext {
                chain_id: ETHEREUM_CHAIN_ID,
                nonce: 7,
                gas_limit,
                max_fee_per_gas: Wei::from_u128(2_000_000_000),
                max_priority_fee_per_gas: Wei::from_u128(1_000_000_000),
            })
        })
    }

    fn receipt<'a>(
        &'a self,
        _id: &'a EthereumTransactionId,
    ) -> EthereumFuture<'a, Result<Option<EthereumReceipt>, SourceError>> {
        Box::pin(async { Ok(None) })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> EthereumFuture<'a, Result<EthereumTransactionId, SourceError>> {
        Box::pin(async move { Ok(transaction.id) })
    }
}

fn offline_error(operation: &str) -> SourceError {
    SourceError {
        message: format!("{operation} is not used by this offline example"),
        retryable: false,
    }
}

fn display_wei(amount: &Wei) -> String {
    amount.checked_to_u128().map_or_else(
        || format!("0x{}", encode_hex(&amount.0)),
        |value| value.to_string(),
    )
}

fn display_usdc(amount: &Wei) -> String {
    amount.checked_to_u128().map_or_else(
        || format!("raw 0x{}", encode_hex(&amount.0)),
        |raw| {
            let whole = raw / 1_000_000;
            let fractional = raw % 1_000_000;
            format!("{whole}.{fractional:06} USDC ({raw} raw units)")
        },
    )
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
