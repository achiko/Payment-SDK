use serde::Deserialize;

use super::provider::{Config, SNAPSHOT_KIND, network_name, transaction_error};
use crate::{Address, Satoshi};
use base::{Decimal, TransactionError, TransactionErrorKind, TransactionSnapshot};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Data {
    scope: Scope,
    source: String,
    asset: Asset,
    transfers: Vec<Transfer>,
    change: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    chain: String,
    network: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    kind: String,
    ticker: String,
    decimals: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    destination: String,
    amount: String,
}

pub(super) fn decode(
    config: &Config,
    wallet_address: &Address,
    snapshot: &TransactionSnapshot,
) -> Result<Vec<(Address, Decimal)>, TransactionError> {
    if snapshot.version() != TransactionSnapshot::VERSION || snapshot.kind() != SNAPSHOT_KIND {
        return Err(invalid("snapshot is not a supported Bitcoin transfer"));
    }
    let data: Data = serde_json::from_value(snapshot.value().clone())
        .map_err(|error| invalid(format!("invalid Bitcoin snapshot: {error}")))?;
    let network = network_name(config.network);
    if data.scope.chain != config.scope.chain.0
        || data.scope.network != config.scope.network
        || data.scope.chain != "bitcoin"
        || data.scope.network != network
        || data.source != wallet_address.encoded()
        || data.change != wallet_address.encoded()
        || data.asset.kind != "native"
        || data.asset.ticker != crate::BTC.ticker
        || data.asset.decimals != crate::BTC.decimals
    {
        return Err(invalid(
            "Bitcoin snapshot does not belong to this wallet, network, or asset",
        ));
    }
    if data.transfers.is_empty() {
        return Err(invalid("Bitcoin snapshot has no transfers"));
    }
    data.transfers
        .into_iter()
        .map(|transfer| {
            let destination = Address::parse_for_network(&transfer.destination, config.network)
                .map_err(invalid)?;
            let amount = transfer.amount.parse::<Decimal>().map_err(invalid)?;
            Satoshi::from_decimal(&amount).map_err(invalid)?;
            Ok((destination, amount))
        })
        .collect()
}

fn invalid(error: impl std::fmt::Display) -> TransactionError {
    transaction_error(TransactionErrorKind::InvalidSnapshot, error)
}

#[cfg(test)]
mod tests {
    use indexing::{ChainId, IndexScope};

    use super::*;
    use crate::{FeeRate, Network};

    const ADDRESS: &str = "1BitcoinEaterAddressDontSendf59kuE";

    fn config() -> Config {
        Config {
            scope: IndexScope {
                chain: ChainId("bitcoin".to_owned()),
                network: "mainnet".to_owned(),
            },
            network: Network::Mainnet,
            address_type: super::super::AddressType::SegwitV0,
            fee_target_blocks: 6,
            max_fee_rate: FeeRate::new(1000),
        }
    }

    fn snapshot(source: &str) -> TransactionSnapshot {
        TransactionSnapshot::new(
            SNAPSHOT_KIND,
            serde_json::json!({
                "scope": { "chain": "bitcoin", "network": "mainnet" },
                "source": source,
                "asset": { "kind": "native", "ticker": "BTC", "decimals": 8 },
                "transfers": [{ "destination": ADDRESS, "amount": "1" }],
                "change": source,
            }),
        )
    }

    #[test]
    fn json_round_trip_restores_transfer_intent() {
        let encoded = serde_json::to_string(&snapshot(ADDRESS)).expect("snapshot must serialize");
        let restored: TransactionSnapshot =
            serde_json::from_str(&encoded).expect("snapshot must deserialize");
        let transfers = decode(&config(), &Address::from_encoded(ADDRESS), &restored)
            .expect("matching wallet must restore its transaction");

        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0].0.encoded(), ADDRESS);
        assert_eq!(transfers[0].1.to_string(), "1");
        assert!(!encoded.contains("private"));
    }

    #[test]
    fn rejects_kind_version_network_and_wallet_mismatches() {
        let wallet = Address::from_encoded(ADDRESS);
        assert!(
            decode(
                &config(),
                &wallet,
                &TransactionSnapshot::new("ethereum.transfer", serde_json::json!({}))
            )
            .is_err()
        );

        let mut encoded = serde_json::to_value(snapshot(ADDRESS)).expect("snapshot must serialize");
        encoded["version"] = serde_json::json!(2);
        let wrong_version = serde_json::from_value(encoded).expect("version remains valid JSON");
        assert!(decode(&config(), &wallet, &wrong_version).is_err());

        let mut encoded = serde_json::to_value(snapshot(ADDRESS)).expect("snapshot must serialize");
        encoded["value"]["scope"]["network"] = serde_json::json!("regtest");
        let wrong_network = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_network).is_err());

        let mut encoded = serde_json::to_value(snapshot(ADDRESS)).expect("snapshot must serialize");
        encoded["value"]["scope"]["chain"] = serde_json::json!("ethereum");
        let wrong_chain = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_chain).is_err());
        assert!(
            decode(
                &config(),
                &Address::from_encoded("1BoatSLRHtKNngkdXEeobR76b53LETtpyT"),
                &snapshot(ADDRESS)
            )
            .is_err()
        );
    }
}
