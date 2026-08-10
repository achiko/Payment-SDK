//! Offline, chain-native Bitcoin wallet example.
//!
//! The example generates two regtest P2WPKH addresses, constructs a transaction
//! that spends a deterministic fictional previous output, and signs it with an
//! ephemeral in-memory signer. It deliberately performs no RPC, preflight, or
//! broadcast operation and never prints private keys or signed transaction
//! bytes.

use std::{error::Error, io};

use chain_bitcoin::{
    BitcoinAddressGenerator, BitcoinAddressKind, BitcoinBuildRequest, BitcoinGenerateAddress,
    BitcoinNetwork, BitcoinOutput, BitcoinTransactionBuilder, BitcoinTransactionCodec,
    BitcoinTransactionId, BitcoinTransactionSigning, BitcoinUtxo, Satoshi, SatoshisPerKvb,
    UnsignedBitcoinTransaction,
};
use chain_contract::DepositAddressGenerator;
use signer::OperationId;
use signer_local::LocalSigner;

const NETWORK: BitcoinNetwork = BitcoinNetwork::Regtest;
const FICTIONAL_PREVIOUS_OUTPUT_VALUE: Satoshi = Satoshi(150_000);
const RECIPIENT_VALUE: Satoshi = Satoshi(100_000);
const FEE_RATE: SatoshisPerKvb = SatoshisPerKvb::new(1_000);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let signer = LocalSigner::ephemeral_for_testing();
    let address_generator = BitcoinAddressGenerator;

    let source = address_generator
        .generate_address(
            BitcoinGenerateAddress::new(
                NETWORK,
                BitcoinAddressKind::SegwitV0,
                OperationId::new("bitcoin-demo-source-address")?,
                "bitcoin-demo-source",
            ),
            &signer,
        )
        .await?;
    let recipient = address_generator
        .generate_address(
            BitcoinGenerateAddress::new(
                NETWORK,
                BitcoinAddressKind::SegwitV0,
                OperationId::new("bitcoin-demo-recipient-address")?,
                "bitcoin-demo-recipient",
            ),
            &signer,
        )
        .await?;

    let fictional_previous_transaction = BitcoinTransactionId([0x42; 32]);
    let source_script = source.address.script_pubkey_for_network(NETWORK)?;
    let selected_output = BitcoinUtxo::from_exact_selection(
        NETWORK,
        &source.address,
        source.key,
        fictional_previous_transaction,
        0,
        FICTIONAL_PREVIOUS_OUTPUT_VALUE,
        source_script.into_bytes(),
    )?;

    let codec = BitcoinTransactionCodec::new(NETWORK);
    let unsigned = BitcoinTransactionBuilder::build(
        &codec,
        BitcoinBuildRequest {
            signing_operation_id: OperationId::new("bitcoin-demo-sign-transaction")?,
            available: vec![selected_output],
            recipients: vec![BitcoinOutput {
                address: recipient.address.clone(),
                value: RECIPIENT_VALUE,
            }],
            change_address: source.address.clone(),
            fee_rate: FEE_RATE,
            drain_wallet: false,
        },
    )?;
    let fee = transaction_fee(&unsigned)?;
    let input_count = unsigned.inputs.len();
    let output_count = unsigned.outputs.len();

    let signed = BitcoinTransactionSigning::sign(&codec, unsigned, &signer).await?;

    println!("Bitcoin network: {}", NETWORK.canonical_name());
    println!("Source P2WPKH address: {}", source.address.0);
    println!("Recipient P2WPKH address: {}", recipient.address.0);
    println!(
        "Fictional previous output: {fictional_previous_transaction}:0 ({} satoshis)",
        FICTIONAL_PREVIOUS_OUTPUT_VALUE.0
    );
    println!(
        "Built transaction: {input_count} input(s), {output_count} output(s), {} satoshis fee at {} sat/kvB",
        fee.0,
        FEE_RATE.satoshis_per_kvb()
    );
    println!("Signed transaction ID: {}", signed.id());
    println!("Signed virtual size: {} vbytes", signed.virtual_size()?);
    println!("No RPC, preflight, or broadcast operation was performed.");
    println!("The fictional previous output does not exist and cannot spend funds.");

    Ok(())
}

fn transaction_fee(transaction: &UnsignedBitcoinTransaction) -> Result<Satoshi, io::Error> {
    let input_total = checked_sum(
        transaction.inputs.iter().map(|input| input.utxo.value.0),
        "Bitcoin demo input total overflowed",
    )?;
    let output_total = checked_sum(
        transaction.outputs.iter().map(|output| output.value.0),
        "Bitcoin demo output total overflowed",
    )?;
    input_total
        .checked_sub(output_total)
        .map(Satoshi)
        .ok_or_else(|| io::Error::other("Bitcoin demo outputs exceed its inputs"))
}

fn checked_sum(
    mut values: impl Iterator<Item = u64>,
    overflow_message: &'static str,
) -> Result<u64, io::Error> {
    values.try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| io::Error::other(overflow_message))
    })
}
