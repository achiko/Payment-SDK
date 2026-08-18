use base::Decimal;
use indexing::{CanonicalAddress, HistoryQuery};
use wallets::{
    AmountFormat, Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, History,
    HistoryAsset, HistoryReader, HistoryRequest,
};

use super::{Wallet, map_error};

impl AmountFormat for Wallet {
    fn display_amount(&self, atomic: &Decimal) -> Result<Decimal, WalletError> {
        let units = atomic
            .to_atomic(0)
            .map_err(|error| map_error(WalletErrorKind::InvalidAmount, error))?;
        Ok(Decimal::from_atomic(units, crate::BTC.decimals))
    }
}

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
                .map_err(|error| map_error(WalletErrorKind::History, error))?;
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
