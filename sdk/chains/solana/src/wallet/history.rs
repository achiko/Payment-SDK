use indexing::{AssetId, CanonicalAddress, HistoryQuery};
use wallets::{
    Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, History, HistoryAsset,
    HistoryReader, HistoryRequest,
};

use crate::AssetKind;

use super::provider::Wallet;

impl<C> HistoryReader for Wallet<C>
where
    C: json_rpc::Client + 'static,
{
    fn history<'a>(&'a self, request: HistoryRequest) -> FutureResult<'a, History> {
        Box::pin(async move {
            let scope = self.config.scope();
            let page = self
                .history
                .history(HistoryQuery {
                    scope: scope.clone(),
                    address: CanonicalAddress {
                        scope: scope.clone(),
                        value: self.key.address().to_string(),
                    },
                    after: request.after,
                    limit: request.limit,
                })
                .await
                .map_err(WalletError::from)?;
            History::from_index(page, scope, |asset| sol_asset(self.config.asset(), asset))
        })
    }
}

fn sol_asset(configured: AssetKind, asset: &AssetId) -> Result<HistoryAsset, WalletError> {
    if asset != &configured.id() {
        return Err(WalletError::new(
            WalletErrorKind::History,
            "Solana history contains an unsupported asset",
        ));
    }
    let metadata = configured.metadata();
    Ok(HistoryAsset {
        id: asset.clone(),
        name: Some(metadata.name.to_owned()),
        ticker: Some(metadata.ticker.to_owned()),
        decimals: metadata.decimals,
    })
}

#[cfg(test)]
mod tests {
    use indexing::{AssetId, ChainId};

    use super::*;

    #[test]
    fn native_history_uses_exact_sol_metadata() {
        let asset = sol_asset(
            AssetKind::Native,
            &AssetId {
                chain: ChainId(crate::CHAIN.to_owned()),
                asset: "native".to_owned(),
            },
        )
        .expect("native SOL asset");

        assert_eq!(asset.name.as_deref(), Some("Solana"));
        assert_eq!(asset.ticker.as_deref(), Some("SOL"));
        assert_eq!(asset.decimals, 9);
    }

    #[test]
    fn rejects_foreign_chain_and_spl_asset_identities() {
        for asset in [
            AssetId {
                chain: ChainId("ethereum".to_owned()),
                asset: "native".to_owned(),
            },
            AssetId {
                chain: ChainId(crate::CHAIN.to_owned()),
                asset: "token-mint".to_owned(),
            },
        ] {
            assert_eq!(
                sol_asset(AssetKind::Native, &asset)
                    .expect_err("unsupported asset")
                    .kind,
                WalletErrorKind::History
            );
        }
    }
}
