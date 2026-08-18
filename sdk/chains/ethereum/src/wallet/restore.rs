use base::{
    Decimal, TransactionBuilder, TransactionError, TransactionErrorKind, TransactionRestore,
    TransactionSnapshot,
};
use serde::Deserialize;

use super::{Builder, SNAPSHOT_KIND, Wallet, transaction_error};
use crate::{Address, AssetKind};

#[derive(Deserialize)]
struct SnapshotScope {
    chain: String,
    network: String,
}

#[derive(Deserialize)]
struct SnapshotAsset {
    kind: String,
    ticker: Option<String>,
    token: Option<String>,
    decimals: u32,
}

#[derive(Deserialize)]
struct SnapshotData {
    scope: SnapshotScope,
    source: String,
    destination: String,
    amount: String,
    asset: SnapshotAsset,
}

impl TransactionRestore for Wallet {
    fn restore(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
        if snapshot.version() != TransactionSnapshot::VERSION || snapshot.kind() != SNAPSHOT_KIND {
            return Err(transaction_error(
                TransactionErrorKind::InvalidSnapshot,
                "snapshot is not a supported Ethereum transfer",
            ));
        }
        let data: SnapshotData = serde_json::from_value(snapshot.value().clone())
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        if data.scope.chain != self.config.scope.chain.0
            || data.scope.network != self.config.scope.network
            || data.source != self.address.to_string()
            || data.asset.decimals != self.config.decimals
            || !asset_matches(&data.asset, &self.config.asset)
        {
            return Err(transaction_error(
                TransactionErrorKind::InvalidSnapshot,
                "Ethereum snapshot does not belong to this wallet, network, or asset",
            ));
        }
        let destination = data
            .destination
            .parse::<Address>()
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        let amount = data
            .amount
            .parse::<Decimal>()
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        amount
            .to_atomic_be_bytes::<32>(self.config.decimals)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        let mut builder = Builder::new(
            self.config.scope.clone(),
            self.address.clone(),
            self.config.asset.clone(),
            self.config.decimals,
            self.signer.clone(),
            self.transactions.clone(),
        );
        builder.transfer = Some((destination, amount));
        builder.validate()?;
        Ok(Box::new(builder))
    }
}

fn asset_matches(snapshot: &SnapshotAsset, configured: &AssetKind) -> bool {
    match configured {
        AssetKind::Native => {
            snapshot.kind == "native"
                && snapshot.ticker.as_deref() == Some(crate::ETH.ticker)
                && snapshot.token.is_none()
        }
        AssetKind::Erc20(token) => {
            snapshot.kind == "erc20"
                && snapshot.ticker.is_none()
                && snapshot.token.as_deref() == Some(token.to_string().as_str())
        }
    }
}
