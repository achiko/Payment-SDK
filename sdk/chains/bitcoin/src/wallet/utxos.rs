use std::{
    collections::BTreeSet,
    sync::{Arc, LazyLock},
};

use indexing::{
    AssetId, CanonicalAddress, ChainId, IndexError, IndexScope, OutputCursor, OutputQuery,
    OutputRequest, OutputSnapshot, SourceError,
};

use crate::{Address, Network, Satoshi, TransactionId, UnspentOutput, UtxoSet, Utxos};

const PAGE_SIZE: usize = 256;
const COINBASE_MATURITY: u64 = 100;
static CHAIN_ID: LazyLock<ChainId> = LazyLock::new(|| ChainId(crate::CHAIN.to_owned()));
static NATIVE_ASSET: LazyLock<AssetId> = LazyLock::new(|| AssetId {
    chain: (*CHAIN_ID).clone(),
    asset: "native".to_owned(),
});

/// Bitcoin wallet view backed by the chain-neutral indexing output capability.
pub struct IndexUtxos {
    scope: IndexScope,
    network: Network,
    outputs: Arc<dyn OutputQuery>,
}

impl IndexUtxos {
    pub fn new(
        scope: IndexScope,
        network: Network,
        outputs: Arc<dyn OutputQuery>,
    ) -> Result<Self, SourceError> {
        if scope.chain != *CHAIN_ID || scope.network != network.canonical_name() {
            return Err(source_error(
                "Bitcoin indexed outputs require the configured Bitcoin scope and network",
                false,
            ));
        }
        Ok(Self {
            scope,
            network,
            outputs,
        })
    }

    async fn load(
        &self,
        address: Address,
        expected: &mut Option<OutputSnapshot>,
        seen: &mut BTreeSet<([u8; 32], u32)>,
    ) -> Result<Vec<UnspentOutput>, SourceError> {
        let canonical = CanonicalAddress {
            scope: self.scope.clone(),
            value: address.encoded().to_owned(),
        };
        let expected_script = address
            .script_pubkey_for_network(self.network)
            .map_err(|error| source_error(error.to_string(), false))?
            .into_bytes();
        let mut after = None;
        let mut outputs = Vec::new();
        loop {
            let page = self
                .outputs
                .outputs(OutputRequest {
                    scope: self.scope.clone(),
                    address: canonical.clone(),
                    after: after.clone(),
                    limit: PAGE_SIZE,
                })
                .await
                .map_err(index_error)?;
            validate_snapshot(expected, &page.snapshot)?;
            let checkpoint = page.snapshot.checkpoint.as_ref().ok_or_else(|| {
                source_error("indexed outputs have no canonical checkpoint", true)
            })?;
            for output in page.outputs {
                if output.address != canonical
                    || output.asset != *NATIVE_ASSET
                    || !output.id.transaction.belongs_to(&self.scope)
                {
                    return Err(source_error(
                        "indexed output does not belong to the requested Bitcoin address and asset",
                        false,
                    ));
                }
                if output.evidence != expected_script {
                    return Err(source_error(
                        "indexed output locking script does not match its Bitcoin address",
                        false,
                    ));
                }
                let transaction_id = output
                    .id
                    .transaction
                    .value
                    .parse::<TransactionId>()
                    .map_err(|_| {
                        source_error(
                            "indexed output has an invalid Bitcoin transaction ID",
                            false,
                        )
                    })?;
                let value = indexed_satoshis(&output.amount)?;
                let confirmations = checkpoint
                    .height
                    .0
                    .checked_sub(output.created_at.0)
                    .and_then(|depth| depth.checked_add(1))
                    .ok_or_else(|| {
                        source_error(
                            "indexed output was created after the canonical checkpoint",
                            false,
                        )
                    })?;
                if output.coinbase && confirmations < COINBASE_MATURITY {
                    continue;
                }
                if !seen.insert((transaction_id.0, output.id.index)) {
                    return Err(source_error("indexed output is duplicated", false));
                }
                outputs.push(UnspentOutput {
                    transaction_id: transaction_id.0,
                    output_index: output.id.index,
                    value,
                    script_pubkey: output.evidence,
                    confirmations,
                    coinbase: output.coinbase,
                });
            }
            let Some(next) = page.next else {
                break;
            };
            validate_cursor(after.as_ref(), &next, &page.snapshot)?;
            after = Some(next);
        }
        Ok(outputs)
    }
}

impl Utxos for IndexUtxos {
    fn utxos<'a>(
        &'a self,
        addresses: Vec<Address>,
    ) -> crate::BoxFuture<'a, Result<UtxoSet, SourceError>> {
        Box::pin(async move {
            if addresses.is_empty() {
                return Err(source_error(
                    "Bitcoin indexed output lookup requires an address",
                    false,
                ));
            }
            let mut snapshot = None;
            let mut seen = BTreeSet::new();
            let mut outputs = Vec::new();
            for address in addresses {
                outputs.extend(self.load(address, &mut snapshot, &mut seen).await?);
            }
            let checkpoint = snapshot.and_then(|value| value.checkpoint).ok_or_else(|| {
                source_error("indexed outputs have no canonical checkpoint", true)
            })?;
            Ok(UtxoSet {
                checkpoint,
                outputs,
            })
        })
    }
}

fn validate_snapshot(
    expected: &mut Option<OutputSnapshot>,
    actual: &OutputSnapshot,
) -> Result<(), SourceError> {
    match expected {
        Some(expected) if expected != actual => Err(source_error(
            "indexed output snapshot changed while loading Bitcoin outputs",
            true,
        )),
        Some(_) => Ok(()),
        None => {
            *expected = Some(actual.clone());
            Ok(())
        }
    }
}

fn validate_cursor(
    previous: Option<&OutputCursor>,
    next: &OutputCursor,
    snapshot: &OutputSnapshot,
) -> Result<(), SourceError> {
    if &next.snapshot != snapshot || previous == Some(next) || next.position.is_empty() {
        return Err(source_error(
            "indexed output query returned an invalid pagination cursor",
            false,
        ));
    }
    Ok(())
}

fn index_error(error: IndexError) -> SourceError {
    source_error(error.message, error.retryable)
}

/// Indexing persists Bitcoin amounts in chain-native atomic units. Unlike the
/// public wallet API, this boundary must not interpret the decimal as BTC and
/// multiply it by `10^8` a second time.
fn indexed_satoshis(amount: &base::Decimal) -> Result<Satoshi, SourceError> {
    amount
        .to_atomic_u64(0)
        .map(Satoshi)
        .map_err(|error| source_error(error.to_string(), false))
}

fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use base::{Decimal, DecimalErrorKind};

    use super::*;

    #[test]
    fn indexed_amount_is_already_satoshis() {
        let amount = Decimal::from(100_000_u64);

        assert_eq!(
            indexed_satoshis(&amount).expect("atomic indexed amount must convert"),
            Satoshi(100_000)
        );
    }

    #[test]
    fn indexed_amount_rejects_fractional_satoshis() {
        let amount = "100000.1"
            .parse::<Decimal>()
            .expect("fixture must be a valid decimal");

        let error = amount
            .to_atomic_u64(0)
            .expect_err("fractional satoshis must be rejected");

        assert_eq!(error.kind, DecimalErrorKind::ExcessPrecision);
        let boundary_error =
            indexed_satoshis(&amount).expect_err("index adapter must reject fractional satoshis");
        assert_eq!(
            boundary_error.message,
            "amount has more than 0 fractional digits"
        );
        assert!(!boundary_error.retryable);
    }
}
