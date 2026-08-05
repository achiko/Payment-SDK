use chain_bitcoin::{
    BitcoinAddressGenerator, BitcoinAddressKind, BitcoinGenerateAddress, BitcoinNetwork,
};
use chain_contract::DepositAddressGenerator;
use futures_executor::block_on;
use signer::OperationId;
use signer_local::LocalSigner;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let keys = LocalSigner::ephemeral_for_testing();
    let generator = BitcoinAddressGenerator;

    println!("Ephemeral Bitcoin regtest wallets");
    for kind in [BitcoinAddressKind::SegwitV0, BitcoinAddressKind::Taproot] {
        let generated = block_on(generator.generate_address(
            BitcoinGenerateAddress::new(
                BitcoinNetwork::Regtest,
                kind,
                OperationId::new(format!("provision-bitcoin-example-{kind:?}"))?,
                "bitcoin-example",
            ),
            &keys,
        ))?;

        println!("\n{kind:?}");
        println!("address: {}", generated.address.0);
        println!("key locator: {:?}", generated.key);
        println!(
            "public key ({:?}): 0x{}",
            generated.public_key.format,
            encode_hex(&generated.public_key.bytes)
        );
    }
    println!("\nprivate keys remain in memory and are lost when this process exits");

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
