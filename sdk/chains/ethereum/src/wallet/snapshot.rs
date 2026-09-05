use serde::Deserialize;

use super::{Builder, SNAPSHOT_KIND, Wallet, WalletConfig, transaction_error};
use crate::{Address, AssetKind};
use base::{Decimal, TransactionError, TransactionErrorKind, TransactionSnapshot};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Data {
    scope: Scope,
    source: String,
    destination: String,
    amount: String,
    asset: Asset,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    chain: String,
    network: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Asset {
    Native { ticker: String, decimals: u32 },
    Erc20 { token: String, decimals: u32 },
}

impl Asset {
    fn matches(&self, configured: &AssetKind, decimals: u32) -> bool {
        match (self, configured) {
            (
                Self::Native {
                    ticker,
                    decimals: actual,
                },
                AssetKind::Native,
            ) => ticker == crate::ETH.ticker && *actual == decimals,
            (
                Self::Erc20 {
                    token,
                    decimals: actual,
                },
                AssetKind::Erc20(configured),
            ) => token == &configured.to_string() && *actual == decimals,
            _ => false,
        }
    }
}

pub(super) fn restore(
    wallet: &Wallet,
    snapshot: &TransactionSnapshot,
) -> Result<Builder, TransactionError> {
    let (destination, amount) = decode(&wallet.config, &wallet.address, snapshot)?;
    let mut builder = Builder::new(
        wallet.config.clone(),
        wallet.address.clone(),
        wallet.signer.clone(),
        wallet.coordinator.clone(),
    );
    builder.transfer = Some((destination, amount));
    builder.validate()?;
    Ok(builder)
}

fn decode(
    config: &WalletConfig,
    wallet_address: &Address,
    snapshot: &TransactionSnapshot,
) -> Result<(Address, Decimal), TransactionError> {
    if snapshot.version() != TransactionSnapshot::VERSION || snapshot.kind() != SNAPSHOT_KIND {
        return Err(invalid("snapshot is not a supported Ethereum transfer"));
    }
    let data: Data = serde_json::from_value(snapshot.value().clone())
        .map_err(|error| invalid(format!("invalid Ethereum snapshot: {error}")))?;
    if data.scope.chain != config.scope.chain.0
        || data.scope.network != config.scope.network
        || data.scope.chain != "ethereum"
        || config.chain_id == 0
        || data.source != wallet_address.to_string()
        || !data.asset.matches(&config.asset, config.decimals)
    {
        return Err(invalid(
            "Ethereum snapshot does not belong to this wallet, network, or asset",
        ));
    }
    let destination = data.destination.parse::<Address>().map_err(invalid)?;
    let amount = data.amount.parse::<Decimal>().map_err(invalid)?;
    amount
        .to_atomic_be_bytes::<32>(config.decimals)
        .map_err(invalid)?;
    Ok((destination, amount))
}

fn invalid(error: impl std::fmt::Display) -> TransactionError {
    transaction_error(TransactionErrorKind::InvalidSnapshot, error)
}

#[cfg(test)]
mod tests {
    use indexing::{ChainId, IndexScope};

    use super::*;

    fn config() -> WalletConfig {
        WalletConfig {
            scope: IndexScope {
                chain: ChainId("ethereum".to_owned()),
                network: "sepolia".to_owned(),
            },
            chain_id: 11_155_111,
            asset: AssetKind::Native,
            decimals: 18,
        }
    }

    fn snapshot(source: &Address) -> TransactionSnapshot {
        TransactionSnapshot::new(
            SNAPSHOT_KIND,
            serde_json::json!({
                "scope": { "chain": "ethereum", "network": "sepolia" },
                "source": source.to_string(),
                "destination": Address([0x22; 20]).to_string(),
                "amount": "1.25",
                "asset": { "kind": "native", "ticker": "ETH", "decimals": 18 },
            }),
        )
    }

    #[test]
    fn json_round_trip_restores_transfer_intent() {
        let wallet = Address([0x11; 20]);
        let encoded = serde_json::to_string(&snapshot(&wallet)).expect("snapshot must serialize");
        let restored = serde_json::from_str(&encoded).expect("snapshot must deserialize");
        let (destination, amount) = decode(&config(), &wallet, &restored)
            .expect("matching wallet must restore its transaction");

        assert_eq!(destination, Address([0x22; 20]));
        assert_eq!(amount.to_string(), "1.25");
        assert!(!encoded.contains("private"));
    }

    #[test]
    fn erc20_json_round_trip_restores_transfer_intent() {
        let wallet = Address([0x11; 20]);
        let token = Address([0xab; 20]);
        let mut config = config();
        config.asset = AssetKind::Erc20(token.clone());
        config.decimals = 6;
        let mut value = snapshot(&wallet).value().clone();
        value["asset"] = serde_json::json!({
            "kind": "erc20", "token": token.to_string(), "decimals": 6,
        });
        let encoded = serde_json::to_string(&TransactionSnapshot::new(SNAPSHOT_KIND, value))
            .expect("snapshot must serialize");
        let restored = serde_json::from_str(&encoded).expect("snapshot must deserialize");

        let (destination, amount) = decode(&config, &wallet, &restored)
            .expect("matching token wallet must restore its transaction");

        assert_eq!(destination, Address([0x22; 20]));
        assert_eq!(amount.to_string(), "1.25");
    }

    #[test]
    fn native_snapshots_reject_asset_mismatches() {
        let wallet = Address([0x11; 20]);
        for (case, asset) in [
            (
                "ticker",
                serde_json::json!({ "kind": "native", "ticker": "BTC", "decimals": 18 }),
            ),
            (
                "ticker casing",
                serde_json::json!({ "kind": "native", "ticker": "eth", "decimals": 18 }),
            ),
            (
                "decimals",
                serde_json::json!({ "kind": "native", "ticker": "ETH", "decimals": 17 }),
            ),
            (
                "token variant",
                serde_json::json!({ "kind": "erc20", "token": Address([0xab; 20]).to_string(), "decimals": 18 }),
            ),
        ] {
            let mut value = snapshot(&wallet).value().clone();
            value["asset"] = asset;
            let error = decode(
                &config(),
                &wallet,
                &TransactionSnapshot::new(SNAPSHOT_KIND, value),
            )
            .expect_err(case);

            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot, "{case}");
            assert_eq!(
                error.message,
                "Ethereum snapshot does not belong to this wallet, network, or asset",
                "{case}"
            );
        }
    }

    #[test]
    fn erc20_snapshots_reject_asset_mismatches() {
        let wallet = Address([0x11; 20]);
        let token = Address([0xab; 20]);
        let mut config = config();
        config.asset = AssetKind::Erc20(token.clone());
        config.decimals = 6;
        for (case, asset) in [
            (
                "token contract",
                serde_json::json!({ "kind": "erc20", "token": Address([0xcd; 20]).to_string(), "decimals": 6 }),
            ),
            (
                "token casing",
                serde_json::json!({ "kind": "erc20", "token": format!("0x{}", "AB".repeat(20)), "decimals": 6 }),
            ),
            (
                "decimals",
                serde_json::json!({ "kind": "erc20", "token": token.to_string(), "decimals": 18 }),
            ),
            (
                "native variant",
                serde_json::json!({ "kind": "native", "ticker": "ETH", "decimals": 6 }),
            ),
        ] {
            let mut value = snapshot(&wallet).value().clone();
            value["asset"] = asset;
            let error = decode(
                &config,
                &wallet,
                &TransactionSnapshot::new(SNAPSHOT_KIND, value),
            )
            .expect_err(case);

            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot, "{case}");
            assert_eq!(
                error.message,
                "Ethereum snapshot does not belong to this wallet, network, or asset",
                "{case}"
            );
        }
    }

    #[test]
    fn wallet_config_rejects_a_zero_token_contract() {
        let mut config = config();
        config.asset = AssetKind::Erc20(Address([0; 20]));
        config.decimals = 6;

        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_kind_version_network_chain_and_wallet_mismatches() {
        let wallet = Address([0x11; 20]);
        assert!(
            decode(
                &config(),
                &wallet,
                &TransactionSnapshot::new("bitcoin.transfer", serde_json::json!({}))
            )
            .is_err()
        );

        let mut encoded = serde_json::to_value(snapshot(&wallet)).expect("snapshot must serialize");
        encoded["version"] = serde_json::json!(2);
        let wrong_version = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_version).is_err());

        let mut encoded = serde_json::to_value(snapshot(&wallet)).expect("snapshot must serialize");
        encoded["value"]["scope"]["chain"] = serde_json::json!("bitcoin");
        let wrong_chain = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_chain).is_err());

        let mut encoded = serde_json::to_value(snapshot(&wallet)).expect("snapshot must serialize");
        encoded["value"]["scope"]["network"] = serde_json::json!("mainnet");
        let wrong_network = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_network).is_err());
        assert!(decode(&config(), &Address([0x33; 20]), &snapshot(&wallet)).is_err());
    }
}
