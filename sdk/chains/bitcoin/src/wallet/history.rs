use indexing::{CanonicalAddress, HistoryQuery};
use wallets::{
    Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, History, HistoryAsset,
    HistoryReader, HistoryRequest,
};

use super::provider::Wallet;

impl HistoryReader for Wallet {
    fn history<'a>(&'a self, request: HistoryRequest) -> FutureResult<'a, History> {
        Box::pin(async move {
            let transactions = self
                .history
                .history(HistoryQuery {
                    scope: self.config.scope.clone(),
                    address: CanonicalAddress {
                        scope: self.config.scope.clone(),
                        value: self.address.to_string(),
                    },
                    after: request.after,
                    limit: request.limit,
                })
                .await
                .map_err(WalletError::from)?;
            History::from_index(transactions, &self.config.scope, bitcoin_asset)
        })
    }
}

fn bitcoin_asset(asset: &indexing::AssetId) -> Result<HistoryAsset, WalletError> {
    if asset.chain.0 != "bitcoin" || asset.asset != "native" {
        return Err(WalletError::new(
            WalletErrorKind::History,
            "Bitcoin history contains an unsupported asset",
        ));
    }
    Ok(HistoryAsset {
        id: asset.clone(),
        name: Some(crate::BTC.name.to_owned()),
        ticker: Some(crate::BTC.ticker.to_owned()),
        decimals: crate::BTC.decimals,
    })
}

#[cfg(test)]
mod tests {
    use indexing::{AssetId, ChainId};

    use super::*;

    #[test]
    fn native_history_uses_bitcoin_display_precision() {
        let asset = bitcoin_asset(&AssetId {
            chain: ChainId(crate::CHAIN.to_owned()),
            asset: "native".to_owned(),
        })
        .expect("native Bitcoin asset");

        assert_eq!(asset.decimals, 8);
        assert_eq!(asset.ticker.as_deref(), Some("BTC"));
    }
}
