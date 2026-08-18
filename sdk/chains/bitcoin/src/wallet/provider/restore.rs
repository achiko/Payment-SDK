use base::{
    Decimal, InputPolicy, TransactionBuilder, TransactionError, TransactionErrorKind,
    TransactionRestore, TransactionSnapshot,
};
use serde::Deserialize;

use super::{SNAPSHOT_KIND, Wallet, transaction_error};
use crate::{Address, Satoshi};

#[derive(Deserialize)]
struct SnapshotScope {
    chain: String,
    network: String,
}

#[derive(Deserialize)]
struct SnapshotAsset {
    kind: String,
    ticker: String,
    decimals: u32,
}

#[derive(Deserialize)]
struct SnapshotTransfer {
    destination: String,
    amount: String,
}

#[derive(Deserialize)]
struct SnapshotData {
    scope: SnapshotScope,
    source: String,
    asset: SnapshotAsset,
    transfers: Vec<SnapshotTransfer>,
    inputs: String,
    change: String,
}

impl TransactionRestore for Wallet {
    fn restore(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
        if snapshot.version() != TransactionSnapshot::VERSION || snapshot.kind() != SNAPSHOT_KIND {
            return Err(transaction_error(
                TransactionErrorKind::InvalidSnapshot,
                "snapshot is not a supported Bitcoin transfer",
            ));
        }
        let data: SnapshotData = serde_json::from_value(snapshot.value().clone())
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        if data.scope.chain != self.config.scope.chain.0
            || data.scope.network != self.config.scope.network
            || data.source != self.address.to_string()
            || data.asset.kind != "native"
            || data.asset.ticker != crate::BTC.ticker
            || data.asset.decimals != crate::BTC.decimals
            || data.transfers.is_empty()
        {
            return Err(transaction_error(
                TransactionErrorKind::InvalidSnapshot,
                "Bitcoin snapshot does not belong to this wallet, network, or asset",
            ));
        }
        let change = Address::parse_for_network(&data.change, self.config.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        let input_policy = match data.inputs.as_str() {
            "automatic" => InputPolicy::Automatic,
            "all" => InputPolicy::SpendAll,
            _ => {
                return Err(transaction_error(
                    TransactionErrorKind::InvalidSnapshot,
                    "Bitcoin snapshot has an unknown input policy",
                ));
            }
        };
        let recipients = data
            .transfers
            .into_iter()
            .map(|transfer| self.restore_transfer(transfer))
            .collect::<Result<Vec<_>, _>>()?;
        let mut builder = self.builder();
        builder.change = change;
        builder.input_policy = input_policy;
        builder.recipients = recipients;
        builder.validate()?;
        Ok(Box::new(builder))
    }
}

impl Wallet {
    fn restore_transfer(
        &self,
        transfer: SnapshotTransfer,
    ) -> Result<(Address, Decimal), TransactionError> {
        let destination = Address::parse_for_network(&transfer.destination, self.config.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        let amount = transfer
            .amount
            .parse::<Decimal>()
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        Satoshi::from_decimal(&amount)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        Ok((destination, amount))
    }
}
