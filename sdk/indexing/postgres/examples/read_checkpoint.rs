//! Step-2 smoke check: read a checkpoint and a retained block over the trait.
/// Chain-neutral scope names: this crate is storage, and `lint.toml` reserves
/// concrete chain vocabulary for the chain crates and the application.
const PRIMARY: &str = "primary";
const OTHER: &str = "other";
const NETWORK: &str = "testing";

use indexing::{BlockHeight, BlockSelector, Blocks, ChainId, IndexScope};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: read_checkpoint <url>")?;
    let scope = IndexScope {
        chain: ChainId(PRIMARY.into()),
        network: NETWORK.into(),
    };
    let repository =
        indexing_postgres::Repository::new(indexing_postgres::pool(&url, 4)?, scope.clone())?;

    println!(
        "tip        = {:?}",
        repository.get(BlockSelector::Tip(scope.clone())).await?
    );
    println!(
        "height4700 = {:?}",
        repository
            .get(BlockSelector::Height {
                scope: scope.clone(),
                height: BlockHeight(4700)
            })
            .await?
    );
    println!(
        "height9999 = {:?}",
        repository
            .get(BlockSelector::Height {
                scope: scope.clone(),
                height: BlockHeight(9999)
            })
            .await?
    );

    let other = IndexScope {
        chain: ChainId(OTHER.into()),
        network: NETWORK.into(),
    };
    println!(
        "wrong scope -> {:?}",
        repository
            .get(BlockSelector::Tip(other))
            .await
            .unwrap_err()
            .kind
    );
    Ok(())
}
