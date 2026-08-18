use bitcoin::hex::DisplayHex;
use indexing::SourceError;
use serde_json::Value;

use crate::{FeeRate, Receipt, Satoshi, SignedTransaction, TransactionId};

use super::{
    Client, Preflight,
    error::{map_json_rpc_error, source_error},
    transport::Client as Transport,
    wire::{
        fee_rate_json, parse_bitcoin_block_hash, parse_btc_amount, parse_header, parse_object,
        required_bool, required_i64, required_string,
    },
};

impl<C> Client<C>
where
    C: Transport,
{
    pub async fn preflight(
        &self,
        transaction: &SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> Result<Preflight, SourceError> {
        let max_fee_rate = fee_rate_json(max_fee_rate)?;
        let raw = self
            .request_result(
                "testmempoolaccept",
                Value::Array(vec![
                    Value::Array(vec![Value::String(
                        transaction.consensus_bytes().to_lower_hex_string(),
                    )]),
                    Value::Number(max_fee_rate),
                ]),
            )
            .await?;
        let values: Vec<Value> = raw.deserialize().map_err(map_json_rpc_error)?;
        if values.len() != 1 {
            return Err(source_error(
                "Bitcoin testmempoolaccept returned an unexpected result count",
                true,
            ));
        }
        let result = values[0].as_object().ok_or_else(|| {
            source_error("Bitcoin testmempoolaccept result must be an object", true)
        })?;
        let returned_id = required_string(result, "txid", "Bitcoin preflight transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin preflight returned an invalid txid", true))?;
        if returned_id != transaction.id() {
            return Err(source_error(
                "Bitcoin preflight returned a different transaction ID",
                true,
            ));
        }
        let allowed = required_bool(result, "allowed", "Bitcoin preflight allowance")?;
        let reject_reason = result
            .get("reject-reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let virtual_size = result.get("vsize").and_then(Value::as_u64);
        let base_fee = result
            .get("fees")
            .and_then(Value::as_object)
            .and_then(|fees| fees.get("base"))
            .map(|value| parse_btc_amount(value, "Bitcoin preflight base fee"))
            .transpose()?
            .map(Satoshi);
        Ok(Preflight {
            allowed,
            reject_reason,
            virtual_size,
            base_fee,
        })
    }

    pub async fn broadcast(
        &self,
        transaction: SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> Result<TransactionId, SourceError> {
        let expected_id = transaction.id();
        let max_fee_rate = fee_rate_json(max_fee_rate)?;
        let raw = self
            .request_result(
                "sendrawtransaction",
                Value::Array(vec![
                    Value::String(transaction.consensus_bytes().to_lower_hex_string()),
                    Value::Number(max_fee_rate),
                ]),
            )
            .await?;
        let returned: String = raw.deserialize().map_err(map_json_rpc_error)?;
        let returned = returned
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin Core returned an invalid transaction ID", true))?;
        if returned != expected_id {
            return Err(source_error(
                "Bitcoin Core returned a different transaction ID after broadcast",
                true,
            ));
        }
        Ok(returned)
    }

    pub async fn receipt(&self, id: &TransactionId) -> Result<Option<Receipt>, SourceError> {
        let raw = match self
            .request_result_detailed(
                "getrawtransaction",
                serde_json::json!([id.to_string(), true]),
            )
            .await
        {
            Ok(raw) => raw,
            Err(failure) if failure.remote_code == Some(-5) => return Ok(None),
            Err(failure) => return Err(failure.error),
        };
        let result = parse_object(&raw, "Bitcoin getrawtransaction result")?;
        let returned = required_string(&result, "txid", "Bitcoin receipt transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin receipt contains an invalid txid", true))?;
        if returned != *id {
            return Err(source_error(
                "Bitcoin receipt contains a different transaction ID",
                true,
            ));
        }
        let mut confirmations = result
            .get("confirmations")
            .map(|value| {
                value.as_i64().ok_or_else(|| {
                    source_error("Bitcoin receipt confirmations are not an integer", true)
                })
            })
            .transpose()?
            .unwrap_or(0);
        let in_active_chain = result
            .get("in_active_chain")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    source_error("Bitcoin receipt active-chain flag is not a boolean", true)
                })
            })
            .transpose()?;
        // With txindex enabled, Core can still return a transaction from an
        // orphaned block after that transaction has also been removed from the
        // mempool. That historical lookup is not evidence of current
        // submission and must not suppress an exact-envelope rebroadcast.
        if confirmations < 0
            || in_active_chain == Some(false)
            || (confirmations == 0 && result.contains_key("blockhash"))
        {
            return Ok(None);
        }
        let included_in = if confirmations > 0 {
            let hash = required_string(&result, "blockhash", "Bitcoin receipt block hash")?;
            let expected_block_hash = parse_bitcoin_block_hash(&hash)?;
            let header = self
                .request_optional_result("getblockheader", serde_json::json!([hash, true]), &[-5])
                .await?;
            let Some(header) = header else {
                return Err(source_error(
                    "Bitcoin receipt block disappeared during header lookup",
                    true,
                ));
            };
            let header_object = parse_object(&header, "Bitcoin receipt block header")?;
            let header_confirmations = required_i64(
                &header_object,
                "confirmations",
                "Bitcoin receipt block confirmations",
            )?;
            if header_confirmations <= 0 {
                return Err(source_error(
                    "Bitcoin receipt block left the canonical chain during lookup",
                    true,
                ));
            }
            let included = parse_header(&header, None)?;
            if included.hash != expected_block_hash {
                return Err(source_error(
                    "Bitcoin receipt header does not match the transaction block hash",
                    true,
                ));
            }
            let canonical = self
                .request_optional_result(
                    "getblockhash",
                    serde_json::json!([included.height.0]),
                    &[-8],
                )
                .await?;
            let Some(canonical) = canonical else {
                return Err(source_error(
                    "Bitcoin receipt height disappeared during canonicality verification",
                    true,
                ));
            };
            let canonical: String = canonical.deserialize().map_err(map_json_rpc_error)?;
            if parse_bitcoin_block_hash(&canonical)? != included.hash {
                return Err(source_error(
                    "Bitcoin receipt block is no longer canonical",
                    true,
                ));
            }
            confirmations = header_confirmations;
            Some(included)
        } else {
            None
        };
        let confirmations = u64::try_from(confirmations.max(0))
            .map_err(|_| source_error("Bitcoin receipt confirmation count exceeds u64", true))?;
        Ok(Some(Receipt {
            id: *id,
            included_in,
            confirmations,
            replaced_by: None,
        }))
    }
}
