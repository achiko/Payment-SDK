use base::BlockRef;
use indexing::{CanonicalAddress, ConfirmationProof, IndexScope, TransactionRef};
use serde::{Serialize, Serializer};

use crate::{History, HistoryAsset, HistoryEntry, HistoryFee, HistoryMovement, HistoryStatus};

#[derive(Serialize)]
struct ScopeRef<'a> {
    chain: &'a str,
    network: &'a str,
}

impl<'a> From<&'a IndexScope> for ScopeRef<'a> {
    fn from(scope: &'a IndexScope) -> Self {
        Self {
            chain: &scope.chain.0,
            network: &scope.network,
        }
    }
}

#[derive(Serialize)]
struct RefWire<'a> {
    scope: ScopeRef<'a>,
    value: &'a str,
}

impl<'a> From<&'a TransactionRef> for RefWire<'a> {
    fn from(value: &'a TransactionRef) -> Self {
        Self {
            scope: (&value.scope).into(),
            value: &value.value,
        }
    }
}

#[derive(Serialize)]
struct AddressWire<'a> {
    scope: ScopeRef<'a>,
    value: &'a str,
}

impl<'a> From<&'a CanonicalAddress> for AddressWire<'a> {
    fn from(value: &'a CanonicalAddress) -> Self {
        Self {
            scope: (&value.scope).into(),
            value: &value.value,
        }
    }
}

#[derive(Serialize)]
struct BlockWire {
    height: u64,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<u64>,
}

impl From<&BlockRef> for BlockWire {
    fn from(value: &BlockRef) -> Self {
        Self {
            height: value.height.0,
            hash: hex::encode(&value.hash.0),
            parent_hash: value.parent_hash.as_ref().map(|hash| hex::encode(&hash.0)),
            timestamp: value.timestamp,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProofWire {
    Depth { required: u64, observed: u64 },
    ChainFinalized,
    DepthAndChainFinalized { required: u64, observed: u64 },
}

impl From<&ConfirmationProof> for ProofWire {
    fn from(value: &ConfirmationProof) -> Self {
        match *value {
            ConfirmationProof::Depth { required, observed } => Self::Depth { required, observed },
            ConfirmationProof::ChainFinalized => Self::ChainFinalized,
            ConfirmationProof::DepthAndChainFinalized { required, observed } => {
                Self::DepthAndChainFinalized { required, observed }
            }
        }
    }
}

#[derive(Serialize)]
struct HistoryWire<'a> {
    transactions: &'a [HistoryEntry],
    next: Option<RefWire<'a>>,
}

impl Serialize for History {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        HistoryWire {
            transactions: &self.transactions,
            next: self.next.as_ref().map(Into::into),
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct EntryWire<'a> {
    scope: ScopeRef<'a>,
    transaction_id: RefWire<'a>,
    revision: u64,
    status: &'a HistoryStatus,
    movements: &'a [HistoryMovement],
    fee: &'a Option<HistoryFee>,
    first_seen_at: u64,
    observed_at: u64,
}

impl Serialize for HistoryEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        EntryWire {
            scope: (&self.scope).into(),
            transaction_id: (&self.transaction_id).into(),
            revision: self.revision.0,
            status: &self.status,
            movements: &self.movements,
            fee: &self.fee,
            first_seen_at: self.first_seen_at,
            observed_at: self.observed_at,
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct AssetWire<'a> {
    chain: &'a str,
    id: &'a str,
    name: &'a Option<String>,
    ticker: &'a Option<String>,
    decimals: u32,
}

impl Serialize for HistoryAsset {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        AssetWire {
            chain: &self.id.chain.0,
            id: &self.id.asset,
            name: &self.name,
            ticker: &self.ticker,
            decimals: self.decimals,
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct MovementWire<'a> {
    id: &'a str,
    kind: &'static str,
    asset: &'a HistoryAsset,
    amount: String,
    from: Option<AddressWire<'a>>,
    to: Option<AddressWire<'a>>,
}

impl Serialize for HistoryMovement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        MovementWire {
            id: &self.id.0,
            kind: movement_kind(self.kind),
            asset: &self.asset,
            amount: self.amount.to_string(),
            from: self.from.as_ref().map(Into::into),
            to: self.to.as_ref().map(Into::into),
        }
        .serialize(serializer)
    }
}

fn movement_kind(kind: indexing::MovementKind) -> &'static str {
    match kind {
        indexing::MovementKind::Transfer => "transfer",
        indexing::MovementKind::Input => "input",
        indexing::MovementKind::Output => "output",
        indexing::MovementKind::InternalTransfer => "internal_transfer",
        indexing::MovementKind::Mint => "mint",
        indexing::MovementKind::Burn => "burn",
    }
}

#[derive(Serialize)]
struct FeeWire<'a> {
    asset: &'a HistoryAsset,
    amount: String,
    payer: Option<AddressWire<'a>>,
}

impl Serialize for HistoryFee {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        FeeWire {
            asset: &self.asset,
            amount: self.amount.to_string(),
            payer: self.payer.as_ref().map(Into::into),
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StatusWire<'a> {
    Pending,
    Included {
        block: BlockWire,
        confirmations: u64,
    },
    Confirmed {
        block: BlockWire,
        proof: ProofWire,
    },
    Failed {
        block: Option<BlockWire>,
        reason: &'a Option<String>,
    },
    Replaced {
        by: RefWire<'a>,
    },
    Dropped,
    Reorged {
        previous_block: BlockWire,
    },
}

impl Serialize for HistoryStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match self {
            Self::Pending => StatusWire::Pending,
            Self::Included {
                block,
                confirmations,
            } => StatusWire::Included {
                block: block.into(),
                confirmations: *confirmations,
            },
            Self::Confirmed { block, proof } => StatusWire::Confirmed {
                block: block.into(),
                proof: proof.into(),
            },
            Self::Failed { block, reason } => StatusWire::Failed {
                block: block.as_ref().map(Into::into),
                reason,
            },
            Self::Replaced { by } => StatusWire::Replaced { by: by.into() },
            Self::Dropped => StatusWire::Dropped,
            Self::Reorged { previous_block } => StatusWire::Reorged {
                previous_block: previous_block.into(),
            },
        };
        wire.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use base::Decimal;
    use indexing::{
        AssetId, ChainId, IndexScope, MovementId, ObservationRevision, ObservedTransaction,
        TransactionPage, TransactionRef, TransactionStatus, ValueMovement,
    };
    use serde_json::json;

    use super::*;
    use crate::{Error, HistoryAsset};

    fn scope(network: &str) -> IndexScope {
        IndexScope {
            chain: ChainId("example".to_owned()),
            network: network.to_owned(),
        }
    }

    fn asset(value: &AssetId) -> Result<HistoryAsset, Error> {
        Ok(HistoryAsset {
            id: value.clone(),
            name: Some("Example Coin".to_owned()),
            ticker: Some("EX".to_owned()),
            decimals: 2,
        })
    }

    fn transaction(transaction_scope: IndexScope) -> ObservedTransaction {
        let asset_id = AssetId {
            chain: transaction_scope.chain.clone(),
            asset: "native".to_owned(),
        };
        ObservedTransaction {
            transaction_id: TransactionRef {
                scope: transaction_scope.clone(),
                value: "tx-1".to_owned(),
            },
            scope: transaction_scope.clone(),
            revision: ObservationRevision(2),
            status: TransactionStatus::Pending,
            movements: vec![ValueMovement::Transfer {
                id: MovementId("movement-1".to_owned()),
                asset: asset_id,
                amount: Decimal::from(125_u64),
                from: CanonicalAddress {
                    scope: transaction_scope.clone(),
                    value: "sender".to_owned(),
                },
                to: CanonicalAddress {
                    scope: transaction_scope,
                    value: "receiver".to_owned(),
                },
            }],
            fee: None,
            first_seen_at: 10,
            observed_at: 11,
        }
    }

    #[test]
    fn serializes_typed_identity_and_display_amounts_readably() {
        let expected = scope("mainnet");
        let history = History::from_index(
            TransactionPage {
                transactions: vec![transaction(expected.clone())],
                next: Some(TransactionRef {
                    scope: expected.clone(),
                    value: "tx-next".to_owned(),
                }),
            },
            &expected,
            asset,
        )
        .expect("scoped history must convert");

        let value = serde_json::to_value(history).expect("history must serialize");
        assert_eq!(
            value["transactions"][0]["scope"],
            json!({
                "chain": "example",
                "network": "mainnet"
            })
        );
        assert_eq!(value["transactions"][0]["movements"][0]["amount"], "1.25");
        assert_eq!(
            value["transactions"][0]["movements"][0]["asset"],
            json!({
                "chain": "example",
                "id": "native",
                "name": "Example Coin",
                "ticker": "EX",
                "decimals": 2
            })
        );
        assert_eq!(
            value["transactions"][0]["movements"][0]["from"]["scope"],
            value["transactions"][0]["scope"]
        );
        assert_eq!(value["next"]["value"], "tx-next");
    }

    #[test]
    fn rejects_a_self_consistent_transaction_from_another_network() {
        let expected = scope("mainnet");
        let error = History::from_index(
            TransactionPage {
                transactions: vec![transaction(scope("testnet"))],
                next: None,
            },
            &expected,
            asset,
        )
        .expect_err("a wallet must not accept another network's history");

        assert_eq!(error.kind, crate::ErrorKind::History);
    }
}
