//! Offline walkthrough of every asynchronous `WalletService` operation for
//! native ETH and an ERC-20 asset.
//!
//! The RPC implementation is an in-memory deterministic double and the signer
//! is ephemeral. This example performs no network calls and must not be adapted
//! into production custody by supplying a funded private key.

use chain_ethereum::{
    BoxFuture as EthereumFuture, Ethereum, EthereumAddress, EthereumAsset, EthereumBuildContext,
    EthereumCollectionRequest, EthereumGenerateAddress, EthereumReceipt, EthereumRpc,
    EthereumSignedTransaction, EthereumTransactionId, EthereumTransferRequest, EthereumWallet, Wei,
};
use indexing::{BlockHash, BlockHeight, BlockRef, SourceError};
use signer::OperationId;
use signer_local::LocalSigner;
use std::{
    collections::BTreeSet,
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use wallet_worker::WalletService;

const DEMO_CHAIN_ID: u64 = 31_337;
const DEMO_TOKEN: EthereumAddress = EthereumAddress([
    0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("static example operation ID must be valid")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("OFFLINE DEMO ONLY - no RPC endpoint or funded key is used.\n");

    let custody = Arc::new(LocalSigner::ephemeral_for_testing());
    let service = WalletService::<Ethereum, _, _, _>::new(
        EthereumWallet::new(DEMO_CHAIN_ID, DemoEthereumRpc::new()),
        Arc::clone(&custody),
        custody,
    );
    let native_asset = EthereumAsset::Native;
    let token_asset = EthereumAsset::Erc20(DEMO_TOKEN);

    // 1. Generate chain-native addresses while custody retains the private keys.
    let native_source = service
        .generate_address(
            &native_asset,
            EthereumGenerateAddress::new(
                DEMO_CHAIN_ID,
                operation("demo-provision-native-source"),
                "demo:native-source",
            ),
        )
        .await?;
    let token_source = service
        .generate_address(
            &token_asset,
            EthereumGenerateAddress::new(
                DEMO_CHAIN_ID,
                operation("demo-provision-token-source"),
                "demo:token-source",
            ),
        )
        .await?;
    let destination = service
        .generate_address(
            &native_asset,
            EthereumGenerateAddress::new(
                DEMO_CHAIN_ID,
                operation("demo-provision-destination"),
                "demo:destination",
            ),
        )
        .await?;
    println!("1. generate_address");
    println!("   native source: {}", native_source.address);
    println!("   token source:  {}", token_source.address);
    println!("   destination:   {}", destination.address);

    // 2. Read native and token balances in their raw atomic units.
    let native_balance = service
        .balance(&native_asset, &native_source.address)
        .await?;
    let token_balance = service.balance(&token_asset, &token_source.address).await?;
    println!("\n2. balance");
    println!(
        "   native spendable: {} wei",
        display_amount(&native_balance.spendable)
    );
    println!(
        "   token spendable:  {} raw token units",
        display_amount(&token_balance.spendable)
    );

    // 3-6. Build, review, sign, broadcast, and read one native transfer.
    let unsigned_native = service
        .build_transfer(
            &native_asset,
            EthereumTransferRequest::native(
                operation("demo-sign-native-transfer"),
                native_source.key,
                native_source.address,
                destination.address.clone(),
                Wei::from_u128(1_000_000_000_000_000),
            ),
        )
        .await?;
    println!("\n3. build_transfer (native ETH)");
    print_review(&unsigned_native);

    let signed_native = service
        .sign_transaction(&native_asset, unsigned_native)
        .await?;
    println!("\n4. sign_transaction");
    println!("   transaction ID: {}", signed_native.id);
    println!("   signed envelope: [not printed]");

    let native_id = service.broadcast(&native_asset, signed_native).await?;
    println!("\n5. broadcast");
    println!("   deterministic RPC accepted: {native_id}");

    let native_receipt = service.transaction(&native_asset, &native_id).await?;
    println!("\n6. transaction");
    print_receipt(native_receipt.as_ref());

    // Repeat the explicit lifecycle with canonical ERC-20 transfer calldata.
    let unsigned_token = service
        .build_transfer(
            &token_asset,
            EthereumTransferRequest::erc20(
                operation("demo-sign-token-transfer"),
                token_source.key.clone(),
                token_source.address.clone(),
                DEMO_TOKEN,
                destination.address.clone(),
                Wei::from_u128(1_000_000),
            ),
        )
        .await?;
    println!("\n3-6. ERC-20 build, sign, broadcast, and receipt");
    print_review(&unsigned_token);
    let signed_token = service
        .sign_transaction(&token_asset, unsigned_token)
        .await?;
    let token_id = service.broadcast(&token_asset, signed_token).await?;
    println!("   transaction ID: {token_id}");
    print_receipt(service.transaction(&token_asset, &token_id).await?.as_ref());

    // 7-8. Check token gas prerequisites, then execute one stateless collection.
    let collection = EthereumCollectionRequest::Token {
        signing_operation_id: operation("demo-sign-token-collection"),
        token: DEMO_TOKEN,
        from: token_source.address,
        key: token_source.key,
        destination: destination.address,
        amount: None,
    };
    let requirements = service
        .collection_requirements(&token_asset, &collection)
        .await?;
    println!("\n7. collection_requirements");
    println!(
        "   native gas deficits: {} (the demo source is sufficiently funded)",
        requirements.len()
    );

    let submission = service.collect(&token_asset, collection).await?;
    println!("\n8. collect");
    println!("   transaction ID: {}", submission.transaction_id);
    for attribution in submission.attribution {
        println!(
            "   attributed debit: {} raw token units from {}",
            display_amount(&attribution.gross_debit),
            attribution.address
        );
    }

    println!("\nNo private key, credential, or signed transaction envelope was printed.");
    Ok(())
}

fn print_review(transaction: &chain_ethereum::UnsignedEthereumTransaction) {
    println!("   chain ID: {}", transaction.chain_id);
    println!("   from: {}", transaction.from);
    println!(
        "   to: {}",
        transaction
            .to
            .as_ref()
            .map_or_else(|| "contract creation".to_owned(), ToString::to_string)
    );
    println!("   value: {}", display_amount(&transaction.value));
    println!("   input bytes: {}", transaction.input.len());
    println!("   nonce: {}", transaction.nonce);
    println!("   gas limit: {}", transaction.gas_limit);
    println!(
        "   maximum fee per gas: {} wei",
        display_amount(&transaction.max_fee_per_gas)
    );
}

fn print_receipt(receipt: Option<&EthereumReceipt>) {
    match receipt {
        Some(receipt) => println!(
            "   included: {}, succeeded: {:?}, confirmations: {}",
            receipt.included_in.is_some(),
            receipt.succeeded,
            receipt.confirmations
        ),
        None => println!("   no receipt is currently available"),
    }
}

fn display_amount(amount: &Wei) -> String {
    amount.checked_to_u128().map_or_else(
        || format!("0x{}", encode_hex(&amount.0)),
        |value| value.to_string(),
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

/// Deterministic in-memory RPC. Broadcast only records a public transaction ID;
/// transaction lookup returns a synthetic receipt only for recorded IDs.
#[derive(Debug)]
struct DemoEthereumRpc {
    next_nonce: AtomicU64,
    broadcast_ids: Mutex<BTreeSet<[u8; 32]>>,
}

impl DemoEthereumRpc {
    fn new() -> Self {
        Self {
            next_nonce: AtomicU64::new(7),
            broadcast_ids: Mutex::new(BTreeSet::new()),
        }
    }
}

impl EthereumRpc for DemoEthereumRpc {
    fn balance<'a>(
        &'a self,
        _address: EthereumAddress,
        asset: &'a EthereumAsset,
        _at: Option<BlockRef>,
    ) -> EthereumFuture<'a, Result<Wei, SourceError>> {
        let amount = match asset {
            EthereumAsset::Native => Wei::from_u128(1_000_000_000_000_000_000),
            EthereumAsset::Erc20(token) if token == &DEMO_TOKEN => Wei::from_u128(25_000_000),
            EthereumAsset::Erc20(_) => Wei::ZERO,
        };
        Box::pin(async move { Ok(amount) })
    }

    fn nonce<'a>(
        &'a self,
        _address: EthereumAddress,
    ) -> EthereumFuture<'a, Result<u64, SourceError>> {
        let nonce = self.next_nonce.load(Ordering::SeqCst);
        Box::pin(async move { Ok(nonce) })
    }

    fn build_context<'a>(
        &'a self,
        request: &'a EthereumTransferRequest,
    ) -> EthereumFuture<'a, Result<EthereumBuildContext, SourceError>> {
        let nonce = self.next_nonce.fetch_add(1, Ordering::SeqCst);
        let gas_limit = if request.data.is_empty() {
            21_000
        } else {
            65_000
        };
        Box::pin(async move {
            Ok(EthereumBuildContext {
                chain_id: DEMO_CHAIN_ID,
                nonce,
                gas_limit,
                max_fee_per_gas: Wei::from_u128(2_000_000_000),
                max_priority_fee_per_gas: Wei::from_u128(1_000_000_000),
            })
        })
    }

    fn receipt<'a>(
        &'a self,
        id: &'a EthereumTransactionId,
    ) -> EthereumFuture<'a, Result<Option<EthereumReceipt>, SourceError>> {
        Box::pin(async move {
            let was_broadcast = self
                .broadcast_ids
                .lock()
                .map_err(|_| demo_source_error("broadcast registry lock was poisoned"))?
                .contains(&id.0);
            if !was_broadcast {
                return Ok(None);
            }
            Ok(Some(EthereumReceipt {
                id: id.clone(),
                included_in: Some(BlockRef {
                    height: BlockHeight(42),
                    hash: BlockHash(vec![0x42; 32]),
                    parent_hash: Some(BlockHash(vec![0x41; 32])),
                    timestamp: Some(1_700_000_000),
                }),
                succeeded: Some(true),
                confirmations: 12,
            }))
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> EthereumFuture<'a, Result<EthereumTransactionId, SourceError>> {
        Box::pin(async move {
            let id = transaction.id;
            self.broadcast_ids
                .lock()
                .map_err(|_| demo_source_error("broadcast registry lock was poisoned"))?
                .insert(id.0);
            Ok(id)
        })
    }
}

fn demo_source_error(message: &str) -> SourceError {
    SourceError {
        message: message.to_owned(),
        retryable: false,
    }
}
