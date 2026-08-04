mod support;

use payment_sdk::{Address, ConnectError, Ethereum, ProviderBuilder};
use support::block_on_ready;

fn main() -> Result<(), ConnectError> {
    block_on_ready(run())
}

async fn run() -> Result<(), ConnectError> {
    // Read-only providers do not require a credential or wallet.
    let provider = ProviderBuilder::<Ethereum>::new()
        .connect_ws("ws://localhost:8546")
        .await?;

    let address = Address::new("0xaccount");
    let result = provider.get_balance(&address);

    assert_eq!(result.message, "ethereum balance requested over WebSocket");
    assert_eq!(result.rpc_message, "WebSocket request sent");
    println!("{}", result.message);
    Ok(())
}
