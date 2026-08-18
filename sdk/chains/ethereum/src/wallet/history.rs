use indexing::{CanonicalAddress, HistoryQuery};
use wallets::{
    Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, History, HistoryAsset,
    HistoryReader, HistoryRequest,
};

use super::{AssetKind, Wallet, wallet_error};

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
                .map_err(|error| wallet_error(WalletErrorKind::History, error))?;
            History::from_index(transactions, &self.config.scope, |asset| {
                ethereum_asset(&self.config, asset)
            })
        })
    }
}

fn ethereum_asset(
    config: &super::WalletConfig,
    asset: &indexing::AssetId,
) -> Result<HistoryAsset, WalletError> {
    if asset.chain.0 != "ethereum" {
        return Err(WalletError::new(
            WalletErrorKind::History,
            "Ethereum history contains a foreign-chain asset",
        ));
    }
    if asset.asset == "native" {
        return Ok(HistoryAsset {
            id: asset.clone(),
            name: Some(crate::ETH.name.to_owned()),
            ticker: Some(crate::ETH.ticker.to_owned()),
            decimals: crate::ETH.decimals,
        });
    }
    match &config.asset {
        AssetKind::Erc20(token) if asset.asset.eq_ignore_ascii_case(&token.to_string()) => {
            Ok(HistoryAsset {
                id: asset.clone(),
                name: None,
                ticker: None,
                decimals: config.decimals,
            })
        }
        _ => Err(WalletError::new(
            WalletErrorKind::History,
            "Ethereum history contains token metadata unavailable to this wallet",
        )),
    }
}

#[cfg(test)]
mod tests {
    use indexing::{AssetId, ChainId, IndexScope};

    use super::*;
    use crate::Address;

    fn config(asset: AssetKind, decimals: u32) -> super::super::WalletConfig {
        super::super::WalletConfig {
            scope: IndexScope {
                chain: ChainId(crate::CHAIN.to_owned()),
                network: "mainnet".to_owned(),
            },
            asset,
            decimals,
        }
    }

    #[test]
    fn native_fees_and_token_movements_use_distinct_precision() {
        let token = Address([7; 20]);
        let config = config(AssetKind::Erc20(token.clone()), 6);
        let native = ethereum_asset(
            &config,
            &AssetId {
                chain: ChainId(crate::CHAIN.to_owned()),
                asset: "native".to_owned(),
            },
        )
        .expect("native fee asset");
        let token = ethereum_asset(
            &config,
            &AssetId {
                chain: ChainId(crate::CHAIN.to_owned()),
                asset: token.to_string(),
            },
        )
        .expect("configured token asset");

        assert_eq!(native.decimals, 18);
        assert_eq!(native.ticker.as_deref(), Some("ETH"));
        assert_eq!(token.decimals, 6);
        assert_eq!(token.ticker, None);
    }
}
