use chain_contract::DepositAddressGenerator;
use chain_ethereum::{EthereumAddressGenerator, EthereumGenerateAddress};
use futures_executor::block_on;
use signer_local::LocalSigner;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let keys = LocalSigner::ephemeral_for_testing();
    let generator = EthereumAddressGenerator;
    let generated = block_on(generator.generate_address(
        EthereumGenerateAddress::new(31_337, "ethereum-example"),
        &keys,
    ))?;

    println!("Ephemeral Ethereum test wallet");
    println!("address: 0x{}", encode_hex(&generated.address.0));
    println!("key locator: {:?}", generated.key);
    println!(
        "public key ({:?}): 0x{}",
        generated.public_key.format,
        encode_hex(&generated.public_key.bytes)
    );
    println!("private key remains in memory and is lost when this process exits");

    Ok(())
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
