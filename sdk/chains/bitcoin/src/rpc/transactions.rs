use base::{TransactionError, TransactionErrorKind, TransactionId as BaseTransactionId};
use bitcoin::hex::DisplayHex;
use indexing::SourceError;
use serde_json::Value;

use crate::{FeeRate, Satoshi, SignedTransaction, TransactionId};

use super::{
    Client, Preflight,
    error::{map_json_rpc_error, source_error},
    transport::Client as Transport,
    wire::{fee_rate_json, parse_btc_amount, required_bool, required_string},
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
    ) -> Result<TransactionId, TransactionError> {
        let expected_id = transaction.id();
        let max_fee_rate = fee_rate_json(max_fee_rate)
            .map_err(|error| transaction_error(TransactionErrorKind::Fee, error))?;
        let raw = match self
            .request_result_detailed_once(
                "sendrawtransaction",
                Value::Array(vec![
                    Value::String(transaction.consensus_bytes().to_lower_hex_string()),
                    Value::Number(max_fee_rate),
                ]),
            )
            .await
        {
            Ok(raw) => raw,
            Err(failure) if failure.remote_code.is_some() => {
                return Err(transaction_error(
                    TransactionErrorKind::Rejected,
                    failure.error,
                ));
            }
            Err(failure) => return Err(ambiguous_submission(expected_id, failure.error)),
        };
        let returned: String = raw
            .deserialize()
            .map_err(map_json_rpc_error)
            .map_err(|error| ambiguous_submission(expected_id, error))?;
        let returned = returned.parse::<TransactionId>().map_err(|_| {
            ambiguous_submission(
                expected_id,
                "Bitcoin Core returned an invalid transaction ID",
            )
        })?;
        if returned != expected_id {
            return Err(ambiguous_submission(
                expected_id,
                "Bitcoin Core returned a different transaction ID after broadcast",
            ));
        }
        Ok(returned)
    }
}

fn transaction_error(
    kind: TransactionErrorKind,
    error: impl std::fmt::Display,
) -> TransactionError {
    TransactionError::new(kind, error.to_string())
}

// design-lint: allow unclassified-free-function -- shared Bitcoin RPC uncertainty mapping attaches the verified local transaction ID independently of client state
fn ambiguous_submission(id: TransactionId, error: impl std::fmt::Display) -> TransactionError {
    transaction_error(TransactionErrorKind::Unavailable, error)
        .with_ambiguous_transaction_id(BaseTransactionId::new(id.to_string()))
}
