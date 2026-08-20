//! Checkpoint-consistent read projections over the committed lifecycle.

use indexing::{
    AssetId, CanonicalPage, CanonicalStatus, CanonicalTransaction, ChainId, HistoryCursor,
    HistoryPosition, HistoryQuery, IndexError, IndexErrorKind, MovementId, MovementKind,
    NetworkFee, OutputCursor, OutputPage, OutputRequest, TransactionRef, Transactions,
    ValueMovement,
};
use tokio_postgres::Row;

use crate::{Repository, row};

const MAX_PAGE: usize = 1_000;

impl Repository {
    pub(crate) async fn list_history(
        &self,
        request: HistoryQuery,
    ) -> Result<CanonicalPage, IndexError> {
        self.check_scope(&request.scope)?;
        self.check_address(&request.address)?;
        validate_limit(request.limit)?;
        let checkpoint = self.read_checkpoint().await?;
        if request
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.checkpoint != checkpoint)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "history changed during pagination",
                true,
            ));
        }

        // One extra row reveals whether another page exists without a count.
        let limit = i64::try_from(request.limit.saturating_add(1))
            .map_err(|_| row::store("page limit exceeds the query range"))?;
        let client = self.pool.get().await.map_err(crate::unavailable)?;
        let (after_height, after_transaction) = match &request.after {
            Some(cursor) => (
                row::as_i64(cursor.position.height.0, "cursor height")?,
                cursor.position.transaction.value.clone(),
            ),
            None => (-1, String::new()),
        };
        let rows = client
            .query(
                "SELECT height, transaction_id, status, failure_reason, block_hash AS hash, \
                 block_parent AS parent, block_timestamp AS timestamp, fee_asset, \
                 fee_amount::text AS fee_amount, fee_payer \
                 FROM history WHERE chain = $1 AND network = $2 AND address = $3 \
                 AND (height, transaction_id) > ($4, $5) \
                 ORDER BY height, transaction_id LIMIT $6",
                &[
                    &request.scope.chain.0,
                    &request.scope.network,
                    &request.address.value,
                    &after_height,
                    &after_transaction,
                    &limit,
                ],
            )
            .await
            .map_err(crate::store)?;

        let has_more = rows.len() > request.limit;
        let mut transactions = Vec::with_capacity(request.limit.min(rows.len()));
        for entry in rows.iter().take(request.limit) {
            transactions.push(self.canonical(&request, entry, &client).await?);
        }
        // The checkpoint must not have moved while the page was assembled, or
        // the page would mix two views of canonical history.
        if self.read_checkpoint().await? != checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "history changed during pagination",
                true,
            ));
        }
        let next = has_more
            .then(|| {
                transactions.last().map(|last| HistoryCursor {
                    checkpoint: checkpoint.clone(),
                    position: HistoryPosition {
                        height: last.block().height,
                        transaction: last.transaction_id.clone(),
                    },
                })
            })
            .flatten();
        Ok(CanonicalPage {
            checkpoint,
            transactions,
            next,
        })
    }

    async fn canonical(
        &self,
        request: &HistoryQuery,
        entry: &Row,
        client: &deadpool_postgres::Client,
    ) -> Result<CanonicalTransaction, IndexError> {
        let height: i64 = entry.try_get("height").map_err(crate::store)?;
        let transaction_id: String = entry.try_get("transaction_id").map_err(crate::store)?;
        let status: String = entry.try_get("status").map_err(crate::store)?;
        let reason: Option<String> = entry.try_get("failure_reason").map_err(crate::store)?;
        let block = row::block(entry, "")?;
        let fee_asset: Option<String> = entry.try_get("fee_asset").map_err(crate::store)?;
        let fee_amount: Option<String> = entry.try_get("fee_amount").map_err(crate::store)?;
        let fee_payer: Option<String> = entry.try_get("fee_payer").map_err(crate::store)?;

        let movements = client
            .query(
                "SELECT kind, movement_id, asset_chain, asset, amount::text AS amount, \
                 from_address, to_address FROM movement \
                 WHERE chain = $1 AND network = $2 AND address = $3 AND height = $4 \
                 AND transaction_id = $5 ORDER BY ordinal",
                &[
                    &request.scope.chain.0,
                    &request.scope.network,
                    &request.address.value,
                    &height,
                    &transaction_id,
                ],
            )
            .await
            .map_err(crate::store)?
            .iter()
            .map(|row| self.movement(row))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CanonicalTransaction {
            scope: request.scope.clone(),
            transaction_id: TransactionRef {
                scope: request.scope.clone(),
                value: transaction_id,
            },
            status: match status.as_str() {
                "included" => CanonicalStatus::Included { block },
                "failed" => CanonicalStatus::Failed { block, reason },
                _ => return Err(row::store("stored transaction status is unknown")),
            },
            movements,
            fee: match (fee_asset, fee_amount) {
                (Some(asset), Some(amount)) => Some(NetworkFee {
                    asset: AssetId {
                        chain: request.scope.chain.clone(),
                        asset,
                    },
                    amount: row::decimal(&amount)?,
                    payer: fee_payer.map(|value| indexing::CanonicalAddress {
                        scope: request.scope.clone(),
                        value,
                    }),
                }),
                _ => None,
            },
        })
    }

    fn movement(&self, entry: &Row) -> Result<ValueMovement, IndexError> {
        let kind: String = entry.try_get("kind").map_err(crate::store)?;
        let amount: String = entry.try_get("amount").map_err(crate::store)?;
        let id = MovementId(entry.try_get("movement_id").map_err(crate::store)?);
        let asset = AssetId {
            chain: ChainId(entry.try_get("asset_chain").map_err(crate::store)?),
            asset: entry.try_get("asset").map_err(crate::store)?,
        };
        let amount = row::decimal(&amount)?;
        let from: Option<String> = entry.try_get("from_address").map_err(crate::store)?;
        let to: Option<String> = entry.try_get("to_address").map_err(crate::store)?;
        let address = |value: Option<String>| {
            value.map(|value| indexing::CanonicalAddress {
                scope: self.scope.clone(),
                value,
            })
        };
        let required = |value: Option<indexing::CanonicalAddress>| {
            value.ok_or_else(|| row::store("stored movement is missing a required endpoint"))
        };
        Ok(match kind.as_str() {
            "transfer" => ValueMovement::Transfer {
                id,
                asset,
                amount,
                from: required(address(from))?,
                to: required(address(to))?,
            },
            "input" => ValueMovement::Input {
                id,
                asset,
                amount,
                owner: address(from),
            },
            "output" => ValueMovement::Output {
                id,
                asset,
                amount,
                owner: address(to),
            },
            "mint" => ValueMovement::Mint {
                id,
                asset,
                amount,
                to: required(address(to))?,
            },
            "burn" => ValueMovement::Burn {
                id,
                asset,
                amount,
                from: required(address(from))?,
            },
            _ => return Err(row::store("stored movement kind is unknown")),
        })
    }

    pub(crate) async fn list_outputs(
        &self,
        request: OutputRequest,
    ) -> Result<OutputPage, IndexError> {
        self.check_scope(&request.scope)?;
        self.check_address(&request.address)?;
        validate_limit(request.limit)?;
        let checkpoint = self.read_checkpoint().await?;
        if request
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.checkpoint != checkpoint)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "outputs changed during pagination",
                true,
            ));
        }
        let after = request
            .after
            .as_ref()
            .map(|cursor| String::from_utf8(cursor.position.clone()))
            .transpose()
            .map_err(|_| row::store("output cursor is not valid"))?
            .unwrap_or_default();
        let limit = i64::try_from(request.limit.saturating_add(1))
            .map_err(|_| row::store("page limit exceeds the query range"))?;
        let client = self.pool.get().await.map_err(crate::unavailable)?;
        let rows = client
            .query(
                "SELECT transaction_id, output_index, address, asset_chain, asset, \
                 amount::text AS amount, evidence, created_at, coinbase FROM output \
                 WHERE chain = $1 AND network = $2 AND address = $3 \
                 AND (transaction_id || ':' || output_index) > $4 \
                 ORDER BY (transaction_id || ':' || output_index) LIMIT $5",
                &[
                    &request.scope.chain.0,
                    &request.scope.network,
                    &request.address.value,
                    &after,
                    &limit,
                ],
            )
            .await
            .map_err(crate::store)?;

        let has_more = rows.len() > request.limit;
        let outputs = rows
            .iter()
            .take(request.limit)
            .map(|entry| row::output(&request.scope, entry))
            .collect::<Result<Vec<_>, _>>()?;
        if self.read_checkpoint().await? != checkpoint {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "outputs changed during pagination",
                true,
            ));
        }
        let next = has_more
            .then(|| {
                outputs.last().map(|last| OutputCursor {
                    checkpoint: checkpoint.clone(),
                    position: format!("{}:{}", last.id.transaction.value, last.id.index)
                        .into_bytes(),
                })
            })
            .flatten();
        Ok(OutputPage {
            checkpoint,
            outputs,
            next,
        })
    }
}

impl Transactions for Repository {
    fn list<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> indexing::BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move { self.list_history(request).await })
    }
}

impl indexing::Outputs for Repository {
    fn list<'a>(
        &'a self,
        request: OutputRequest,
    ) -> indexing::BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move { self.list_outputs(request).await })
    }
}

fn validate_limit(limit: usize) -> Result<(), IndexError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(IndexError::new(
            IndexErrorKind::InvalidRequest,
            "page limit must be between one and one thousand",
            false,
        ));
    }
    Ok(())
}

/// Unused today but kept so the movement mapper can name every variant.
#[allow(dead_code)]
const fn kinds() -> [MovementKind; 5] {
    [
        MovementKind::Transfer,
        MovementKind::Input,
        MovementKind::Output,
        MovementKind::Mint,
        MovementKind::Burn,
    ]
}
