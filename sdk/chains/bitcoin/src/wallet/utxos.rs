use std::{
    collections::BTreeSet,
    sync::{Arc, LazyLock},
};

use indexing::{
    AssetId, BlockRef, CanonicalAddress, ChainId, IndexScope, OutputCursor, OutputRequest, Outputs,
    SourceError,
};

use crate::{Address, Network, Satoshi, TransactionId, UnspentOutput, UtxoSet};

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
    outputs: Arc<dyn Outputs>,
}

impl IndexUtxos {
    pub fn new(
        scope: IndexScope,
        network: Network,
        outputs: Arc<dyn Outputs>,
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
        expected_checkpoint: &mut Option<BlockRef>,
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
                .list(OutputRequest {
                    scope: self.scope.clone(),
                    address: canonical.clone(),
                    after: after.clone(),
                    limit: PAGE_SIZE,
                })
                .await
                .map_err(|error| source_error(error.message, error.retryable))?;
            let checkpoint = page.checkpoint.as_ref().ok_or_else(|| {
                source_error("indexed outputs have no canonical checkpoint", true)
            })?;
            if expected_checkpoint.get_or_insert_with(|| checkpoint.clone()) != checkpoint {
                return Err(source_error(
                    "indexed output checkpoint changed while loading Bitcoin outputs",
                    true,
                ));
            }
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
                let value = Satoshi::from_indexed_amount(&output.amount)?;
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
            validate_cursor(after.as_ref(), &next, &page.checkpoint)?;
            after = Some(next);
        }
        Ok(outputs)
    }
}

impl IndexUtxos {
    pub async fn utxos(&self, addresses: Vec<Address>) -> Result<UtxoSet, SourceError> {
        if addresses.is_empty() {
            return Err(source_error(
                "Bitcoin indexed output lookup requires an address",
                false,
            ));
        }
        let mut checkpoint = None;
        let mut seen = BTreeSet::new();
        let mut outputs = Vec::new();
        for address in addresses {
            outputs.extend(self.load(address, &mut checkpoint, &mut seen).await?);
        }
        let checkpoint = checkpoint
            .ok_or_else(|| source_error("indexed outputs have no canonical checkpoint", true))?;
        Ok(UtxoSet {
            checkpoint,
            outputs,
        })
    }
}

fn validate_cursor(
    previous: Option<&OutputCursor>,
    next: &OutputCursor,
    checkpoint: &Option<BlockRef>,
) -> Result<(), SourceError> {
    if &next.checkpoint != checkpoint || previous == Some(next) || next.position.is_empty() {
        return Err(source_error(
            "indexed output query returned an invalid pagination cursor",
            false,
        ));
    }
    Ok(())
}

impl Satoshi {
    /// Indexing persists Bitcoin amounts in chain-native atomic units. Unlike the
    /// public wallet API, this boundary must not interpret the decimal as BTC and
    /// multiply it by `10^8` a second time.
    fn from_indexed_amount(amount: &base::Decimal) -> Result<Self, SourceError> {
        amount
            .to_atomic_u64(0)
            .map(Self)
            .map_err(|error| source_error(error.to_string(), false))
    }
}

// design-lint: allow unclassified-free-function -- indexed Bitcoin output boundary constructs foreign SourceError with caller-selected retryability across amount, checkpoint and pagination validation without coupling wallet reads to RPC plumbing
fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use base::{Decimal, DecimalErrorKind};
    use futures_executor::block_on;
    use indexing::{BoxFuture, IndexError, IndexErrorKind, OutputPage};

    use super::*;

    struct FailingOutputs(IndexError);

    impl Outputs for FailingOutputs {
        fn list<'a>(
            &'a self,
            _request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async { Err(self.0.clone()) })
        }
    }

    #[test]
    fn indexed_output_errors_preserve_message_and_retryability() {
        for retryable in [false, true] {
            let expected = IndexError::new(
                IndexErrorKind::Store,
                " indexing store failed: request 7\nretained context ",
                retryable,
            );
            let outputs = IndexUtxos::new(
                IndexScope {
                    chain: ChainId(crate::CHAIN.to_owned()),
                    network: "mainnet".to_owned(),
                },
                Network::Mainnet,
                Arc::new(FailingOutputs(expected.clone())),
            )
            .expect("matching scope");
            let address = Address::from_encoded("1BitcoinEaterAddressDontSendf59kuE");

            let error = block_on(outputs.utxos(vec![address]))
                .expect_err("indexed output failure must reach the wallet caller");

            assert_eq!(error.message, expected.message);
            assert_eq!(error.retryable, retryable);
        }
    }

    fn checkpoint(height: u64) -> BlockRef {
        BlockRef {
            position: indexing::BlockPosition(height),
            height: indexing::BlockHeight(height),
            hash: indexing::BlockHash(vec![height as u8]),
            parent: None,
            timestamp: None,
        }
    }

    struct ScriptedOutputs(Mutex<VecDeque<(Option<OutputCursor>, OutputPage)>>);

    impl Outputs for ScriptedOutputs {
        fn list<'a>(
            &'a self,
            request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            let (after, page) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("expected output read");
            assert_eq!(request.after, after);
            Box::pin(async { Ok(page) })
        }
    }

    #[test]
    fn output_pages_and_addresses_must_keep_one_checkpoint() {
        let first = checkpoint(10);
        let mut reorg = first.clone();
        reorg.hash = indexing::BlockHash(vec![99]);
        for last in [first.clone(), checkpoint(11), reorg] {
            let cursor = OutputCursor {
                checkpoint: Some(first.clone()),
                position: vec![1],
            };
            let pages = Arc::new(ScriptedOutputs(Mutex::new(VecDeque::from([
                (
                    None,
                    OutputPage {
                        checkpoint: Some(first.clone()),
                        outputs: Vec::new(),
                        next: Some(cursor.clone()),
                    },
                ),
                (
                    Some(cursor),
                    OutputPage {
                        checkpoint: Some(first.clone()),
                        outputs: Vec::new(),
                        next: None,
                    },
                ),
                (
                    None,
                    OutputPage {
                        checkpoint: Some(last.clone()),
                        outputs: Vec::new(),
                        next: None,
                    },
                ),
            ]))));
            let outputs = IndexUtxos::new(
                IndexScope {
                    chain: ChainId(crate::CHAIN.to_owned()),
                    network: "mainnet".to_owned(),
                },
                Network::Mainnet,
                pages.clone(),
            )
            .unwrap();
            let addresses = vec![
                Address::from_encoded("1BitcoinEaterAddressDontSendf59kuE"),
                Address::from_encoded("1BoatSLRHtKNngkdXEeobR76b53LETtpyT"),
            ];
            let result = block_on(outputs.utxos(addresses));
            assert!(pages.0.lock().unwrap().is_empty());
            if last == first {
                assert_eq!(result.unwrap().checkpoint, first);
                continue;
            }
            let error = result.expect_err("checkpoint changes must restart the entire read");
            assert!(error.retryable);
            assert_eq!(
                error.message,
                "indexed output checkpoint changed while loading Bitcoin outputs"
            );
        }
    }

    #[test]
    fn output_cursor_must_match_page_checkpoint_and_advance() {
        let page_checkpoint = Some(checkpoint(10));
        let cursor = OutputCursor {
            checkpoint: page_checkpoint.clone(),
            position: vec![1],
        };
        validate_cursor(None, &cursor, &page_checkpoint).expect("advancing cursor must be valid");

        let wrong_checkpoint = OutputCursor {
            checkpoint: Some(checkpoint(11)),
            position: vec![1],
        };
        assert!(validate_cursor(None, &wrong_checkpoint, &page_checkpoint).is_err());
        assert!(validate_cursor(Some(&cursor), &cursor, &page_checkpoint).is_err());
        let empty = OutputCursor {
            checkpoint: page_checkpoint.clone(),
            position: Vec::new(),
        };
        assert!(validate_cursor(None, &empty, &page_checkpoint).is_err());
    }

    #[test]
    fn indexed_amount_is_already_satoshis() {
        let amount = Decimal::from(100_000_u64);

        assert_eq!(
            Satoshi::from_indexed_amount(&amount).expect("atomic indexed amount must convert"),
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
        let boundary_error = Satoshi::from_indexed_amount(&amount)
            .expect_err("index adapter must reject fractional satoshis");
        assert_eq!(
            boundary_error.message,
            "amount has more than 0 fractional digits"
        );
        assert!(!boundary_error.retryable);
    }

    #[test]
    fn indexed_amount_preserves_zero_maximum_and_rejects_invalid_integers() {
        for amount in [0, u64::MAX] {
            assert_eq!(
                Satoshi::from_indexed_amount(&Decimal::from(amount)).unwrap(),
                Satoshi(amount)
            );
        }
        for (amount, message) in [
            ("-1", "currency amount must not be negative"),
            (
                "18446744073709551616",
                "atomic amount exceeds the u64 range",
            ),
        ] {
            let error = Satoshi::from_indexed_amount(&amount.parse().unwrap()).unwrap_err();
            assert_eq!(error.message, message);
            assert!(!error.retryable);
        }
    }
}
