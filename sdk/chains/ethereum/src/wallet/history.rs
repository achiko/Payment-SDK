use indexing::{CanonicalAddress, HistoryQuery};
use wallets::{
    Error as WalletError, ErrorKind as WalletErrorKind, FutureResult, History, HistoryAsset,
    HistoryReader, HistoryRequest, HistoryStatus,
};

use super::{AssetKind, Wallet};

impl HistoryReader for Wallet {
    fn history<'a>(&'a self, request: HistoryRequest) -> FutureResult<'a, History> {
        Box::pin(async move {
            let address = CanonicalAddress {
                scope: self.config.scope.clone(),
                value: self.address.to_string(),
            };
            let transactions = self
                .history
                .history(HistoryQuery {
                    scope: self.config.scope.clone(),
                    address: address.clone(),
                    after: request.after,
                    limit: request.limit,
                })
                .await
                .map_err(WalletError::from)?;
            let history = History::from_index(transactions, &self.config.scope, |asset| {
                ethereum_asset(&self.config, asset)
            })?;
            self.config.selected_history(&address, history)
        })
    }
}

impl super::WalletConfig {
    fn selected_history(
        &self,
        wallet: &CanonicalAddress,
        mut history: History,
    ) -> Result<History, WalletError> {
        if history.transactions.iter().any(|transaction| {
            transaction.fee.as_ref().is_some_and(|fee| {
                fee.asset.id.chain != self.scope.chain || fee.asset.id.asset != "native"
            })
        }) {
            return Err(WalletError::new(
                WalletErrorKind::History,
                "Ethereum history contains a non-native network fee",
            ));
        }
        history.transactions.retain_mut(|transaction| {
            transaction
                .movements
                .retain(|movement| self.selects(&movement.asset.id));
            !transaction.movements.is_empty()
                || matches!(transaction.status, HistoryStatus::Failed { .. })
                    && transaction.fee.as_ref().and_then(|fee| fee.payer.as_ref()) == Some(wallet)
        });
        Ok(history)
    }

    fn selects(&self, asset: &indexing::AssetId) -> bool {
        if asset.chain != self.scope.chain {
            return false;
        }
        match &self.asset {
            AssetKind::Native => asset.asset == "native",
            AssetKind::Erc20(token) => asset.asset.eq_ignore_ascii_case(&token.to_string()),
        }
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
    let token = asset.asset.parse::<crate::Address>().map_err(|_| {
        WalletError::new(
            WalletErrorKind::History,
            "Ethereum history contains an invalid ERC-20 asset identity",
        )
    })?;
    if token.is_zero() {
        return Err(WalletError::new(
            WalletErrorKind::History,
            "Ethereum history contains a zero ERC-20 asset identity",
        ));
    }
    let decimals = match &config.asset {
        AssetKind::Erc20(token) if asset.asset.eq_ignore_ascii_case(&token.to_string()) => {
            config.decimals
        }
        _ => 0,
    };
    Ok(HistoryAsset {
        id: asset.clone(),
        name: None,
        ticker: None,
        decimals,
    })
}

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockPosition, BlockRef, Decimal};
    use indexing::{
        AssetId, ChainId, HistoryCursor, HistoryPosition, IndexScope, MovementId, NetworkFee,
        ObservedTransaction, TransactionPage, TransactionRef, TransactionStatus, ValueMovement,
    };

    use super::*;
    use crate::Address;

    fn config(asset: AssetKind, decimals: u32) -> super::super::WalletConfig {
        super::super::WalletConfig {
            scope: IndexScope {
                chain: ChainId(crate::CHAIN.to_owned()),
                network: "mainnet".to_owned(),
            },
            chain_id: 1,
            asset,
            decimals,
        }
    }

    fn scope() -> IndexScope {
        config(AssetKind::Native, crate::ETH.decimals).scope
    }

    fn address(value: u8) -> CanonicalAddress {
        CanonicalAddress {
            scope: scope(),
            value: Address([value; 20]).to_string(),
        }
    }

    fn asset(value: impl Into<String>) -> AssetId {
        AssetId {
            chain: ChainId(crate::CHAIN.to_owned()),
            asset: value.into(),
        }
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8; 32]),
            parent: None,
            timestamp: None,
        }
    }

    fn movement(id: &str, asset: AssetId) -> ValueMovement {
        ValueMovement::Transfer {
            id: MovementId(id.to_owned()),
            asset,
            amount: Decimal::from(1_u64),
            from: address(1),
            to: address(2),
        }
    }

    fn transaction(
        id: &str,
        status: TransactionStatus,
        movements: Vec<ValueMovement>,
        payer: Option<CanonicalAddress>,
    ) -> ObservedTransaction {
        ObservedTransaction {
            scope: scope(),
            transaction_id: TransactionRef {
                scope: scope(),
                value: id.to_owned(),
            },
            status,
            movements,
            fee: Some(NetworkFee {
                asset: asset("native"),
                amount: Decimal::from(21_000_u64),
                payer,
            }),
        }
    }

    fn present(
        config: &super::super::WalletConfig,
        wallet: &CanonicalAddress,
        page: TransactionPage,
    ) -> Result<History, WalletError> {
        let history =
            History::from_index(page, &config.scope, |value| ethereum_asset(config, value))?;
        config.selected_history(wallet, history)
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

    #[test]
    fn token_history_keeps_only_configured_token_and_its_native_fee() {
        let token = Address([7; 20]);
        let config = config(AssetKind::Erc20(token.clone()), 6);
        let history = present(
            &config,
            &address(1),
            TransactionPage {
                checkpoint: Some(block(9)),
                transactions: vec![transaction(
                    "mixed",
                    TransactionStatus::Included {
                        block: block(9),
                        confirmations: 1,
                    },
                    vec![
                        movement("native", asset("native")),
                        movement("selected", asset(token.to_string())),
                        movement("unrelated", asset(Address([8; 20]).to_string())),
                    ],
                    Some(address(1)),
                )],
                next: None,
            },
        )
        .expect("valid Ethereum facts must be projected");

        assert_eq!(history.transactions.len(), 1);
        assert_eq!(history.transactions[0].movements.len(), 1);
        assert_eq!(history.transactions[0].movements[0].id.0, "selected");
        assert_eq!(
            history.transactions[0]
                .fee
                .as_ref()
                .map(|fee| fee.asset.id.asset.as_str()),
            Some("native")
        );
        assert_eq!(history.transactions[0].movements[0].asset.decimals, 6);
        assert_eq!(
            history.transactions[0]
                .fee
                .as_ref()
                .expect("token transaction fee")
                .asset
                .ticker
                .as_deref(),
            Some("ETH")
        );
    }

    #[test]
    fn native_history_filters_tokens_and_preserves_raw_page_boundaries() {
        let checkpoint = Some(block(12));
        let next = Some(HistoryCursor {
            checkpoint: checkpoint.clone(),
            position: HistoryPosition {
                height: BlockHeight(11),
                transaction: TransactionRef {
                    scope: scope(),
                    value: "next".to_owned(),
                },
            },
        });
        let config = config(AssetKind::Native, crate::ETH.decimals);
        let history = present(
            &config,
            &address(1),
            TransactionPage {
                checkpoint: checkpoint.clone(),
                transactions: vec![
                    transaction(
                        "mixed",
                        TransactionStatus::Included {
                            block: block(12),
                            confirmations: 1,
                        },
                        vec![
                            movement("native", asset("native")),
                            movement("token", asset(Address([7; 20]).to_string())),
                        ],
                        Some(address(1)),
                    ),
                    transaction(
                        "token-only",
                        TransactionStatus::Included {
                            block: block(11),
                            confirmations: 2,
                        },
                        vec![movement("token", asset(Address([7; 20]).to_string()))],
                        Some(address(1)),
                    ),
                ],
                next: next.clone(),
            },
        )
        .expect("same-chain unrelated assets are valid before projection");

        assert_eq!(history.checkpoint, checkpoint);
        assert_eq!(history.next, next);
        assert_eq!(history.transactions.len(), 1);
        assert_eq!(history.transactions[0].movements.len(), 1);
        assert_eq!(history.transactions[0].movements[0].id.0, "native");

        let empty = present(
            &config,
            &address(1),
            TransactionPage {
                checkpoint: history.checkpoint.clone(),
                transactions: vec![transaction(
                    "token-only",
                    TransactionStatus::Included {
                        block: block(11),
                        confirmations: 2,
                    },
                    vec![movement("token", asset(Address([7; 20]).to_string()))],
                    Some(address(1)),
                )],
                next: history.next.clone(),
            },
        )
        .expect("unrelated token transaction must validate before projection");
        assert!(empty.transactions.is_empty());
        assert_eq!(empty.checkpoint, history.checkpoint);
        assert_eq!(empty.next, history.next);
    }

    #[test]
    fn fee_only_failure_is_visible_only_to_its_payer() {
        let wallet = address(1);
        let config = config(AssetKind::Erc20(Address([7; 20])), 6);
        let history = present(
            &config,
            &wallet,
            TransactionPage {
                checkpoint: Some(block(5)),
                transactions: vec![
                    transaction(
                        "ours",
                        TransactionStatus::Failed {
                            block: block(5),
                            reason: Some("reverted".to_owned()),
                        },
                        Vec::new(),
                        Some(wallet.clone()),
                    ),
                    transaction(
                        "theirs",
                        TransactionStatus::Failed {
                            block: block(5),
                            reason: Some("reverted".to_owned()),
                        },
                        Vec::new(),
                        Some(address(2)),
                    ),
                    transaction(
                        "included-fee-only",
                        TransactionStatus::Included {
                            block: block(5),
                            confirmations: 1,
                        },
                        Vec::new(),
                        Some(wallet.clone()),
                    ),
                ],
                next: None,
            },
        )
        .expect("valid fee-only failures must project");

        assert_eq!(history.transactions.len(), 1);
        assert_eq!(history.transactions[0].transaction_id.value, "ours");
        assert!(history.transactions[0].movements.is_empty());
        assert!(history.transactions[0].fee.is_some());
    }

    #[test]
    fn foreign_asset_cannot_be_hidden_by_projection() {
        let config = config(AssetKind::Native, crate::ETH.decimals);
        let mut foreign = asset("foreign-token");
        foreign.chain = ChainId("bitcoin".to_owned());
        let error = present(
            &config,
            &address(1),
            TransactionPage {
                checkpoint: Some(block(4)),
                transactions: vec![transaction(
                    "foreign",
                    TransactionStatus::Included {
                        block: block(4),
                        confirmations: 1,
                    },
                    vec![movement("foreign", foreign)],
                    Some(address(1)),
                )],
                next: None,
            },
        )
        .expect_err("foreign facts must fail before asset projection");

        assert_eq!(error.kind, WalletErrorKind::History);
        assert!(error.message.contains("does not belong"));
    }

    #[test]
    fn corrupt_transaction_identity_cannot_be_hidden_by_projection() {
        let config = config(AssetKind::Native, crate::ETH.decimals);
        let mut observed = transaction(
            "corrupt",
            TransactionStatus::Included {
                block: block(4),
                confirmations: 1,
            },
            vec![movement("unrelated", asset(Address([8; 20]).to_string()))],
            Some(address(1)),
        );
        observed.transaction_id.scope.network = "sepolia".to_owned();

        let error = present(
            &config,
            &address(1),
            TransactionPage {
                checkpoint: Some(block(4)),
                transactions: vec![observed],
                next: None,
            },
        )
        .expect_err("corrupt identity must fail before asset projection");

        assert_eq!(error.kind, WalletErrorKind::History);
        assert!(error.message.contains("identity"));
    }

    #[test]
    fn malformed_same_chain_token_cannot_be_hidden_by_projection() {
        let config = config(AssetKind::Native, crate::ETH.decimals);
        let error = present(
            &config,
            &address(1),
            TransactionPage {
                checkpoint: Some(block(4)),
                transactions: vec![transaction(
                    "malformed-token",
                    TransactionStatus::Included {
                        block: block(4),
                        confirmations: 1,
                    },
                    vec![movement("malformed", asset("not-a-contract"))],
                    Some(address(1)),
                )],
                next: None,
            },
        )
        .expect_err("malformed token identity must fail before asset projection");

        assert_eq!(error.kind, WalletErrorKind::History);
        assert!(error.message.contains("invalid ERC-20 asset identity"));
    }

    #[test]
    fn non_native_network_fee_is_rejected_before_projection() {
        let token = Address([7; 20]);
        let config = config(AssetKind::Erc20(token.clone()), 6);
        let mut observed = transaction(
            "bad-fee",
            TransactionStatus::Included {
                block: block(4),
                confirmations: 1,
            },
            vec![movement("unrelated", asset(Address([8; 20]).to_string()))],
            Some(address(1)),
        );
        observed.fee.as_mut().expect("fixture fee").asset = asset(token.to_string());

        let error = present(
            &config,
            &address(1),
            TransactionPage {
                checkpoint: Some(block(4)),
                transactions: vec![observed],
                next: None,
            },
        )
        .expect_err("Ethereum network fees must always use the native asset");

        assert_eq!(error.kind, WalletErrorKind::History);
        assert_eq!(
            error.message,
            "Ethereum history contains a non-native network fee"
        );
    }
}
