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

pub(super) fn restore(
    wallet: &Wallet,
    snapshot: &TransactionSnapshot,
) -> Result<Builder, TransactionError> {
    let (destination, amount) = decode(&wallet.config, &wallet.address, snapshot)?;
    let mut builder = Builder::new(
        wallet.config.scope.clone(),
        wallet.config.chain_id,
        wallet.address.clone(),
        wallet.config.asset.clone(),
        wallet.config.decimals,
        wallet.signer.clone(),
        wallet.transactions.clone(),
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
        || !asset_matches(&data.asset, &config.asset, config.decimals)
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

fn asset_matches(snapshot: &Asset, configured: &AssetKind, decimals: u32) -> bool {
    match (snapshot, configured) {
        (
            Asset::Native {
                ticker,
                decimals: actual,
            },
            AssetKind::Native,
        ) => ticker == crate::ETH.ticker && *actual == decimals,
        (
            Asset::Erc20 {
                token,
                decimals: actual,
            },
            AssetKind::Erc20(configured),
        ) => token == &configured.to_string() && *actual == decimals,
        _ => false,
    }
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
