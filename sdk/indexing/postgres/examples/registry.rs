//! Step-3 smoke check: register addresses and read the selection back.
/// Chain-neutral scope names: this crate is storage, and `lint.toml` reserves
/// concrete chain vocabulary for the chain crates and the application.
const PRIMARY: &str = "primary";
const OTHER: &str = "other";
const NETWORK: &str = "testing";

use indexing::{
    AddressFilter, BlockPosition, CanonicalAddress, ChainId, IndexScope, RegisteredAddress,
    Registry,
};

fn entry(scope: &IndexScope, id: &str, address: &str, height: u64) -> RegisteredAddress {
    RegisteredAddress {
        id: id.to_owned(),
        filter: AddressFilter {
            address: CanonicalAddress {
                scope: scope.clone(),
                value: address.to_owned(),
            },
            start_position: BlockPosition(height),
        },
        material: vec![0xde, 0xad, 0xbe, 0xef],
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).ok_or("usage: registry <url>")?;
    let scope = IndexScope {
        chain: ChainId(PRIMARY.into()),
        network: NETWORK.into(),
    };
    let repository =
        indexing_postgres::Repository::new(indexing_postgres::pool(&url, 4)?, scope.clone())?;

    repository
        .register(entry(&scope, "deposit-1", "bcrt1qaaa", 4711))
        .await?;
    repository
        .register(entry(&scope, "deposit-2", "bcrt1qbbb", 4720))
        .await?;

    // material is redacted in Debug, so key bytes cannot leak into logs.
    for found in repository.registered(&scope).await? {
        println!("{found:?}");
    }

    let duplicate = repository
        .register(entry(&scope, "deposit-1", "bcrt1qccc", 4730))
        .await
        .unwrap_err();
    println!("duplicate id      -> {:?}", duplicate.kind);

    let same_address = repository
        .register(entry(&scope, "deposit-3", "bcrt1qaaa", 4740))
        .await
        .unwrap_err();
    println!("duplicate address -> {:?}", same_address.kind);

    let other = IndexScope {
        chain: ChainId(OTHER.into()),
        network: NETWORK.into(),
    };
    println!(
        "foreign scope     -> {:?}",
        repository.registered(&other).await.unwrap_err().kind
    );
    Ok(())
}
