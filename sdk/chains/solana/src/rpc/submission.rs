use std::str::FromStr;

use base::{TransactionError, TransactionErrorKind, TransactionId};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::json;
use solana_hash::Hash;
use solana_signature::Signature;

use crate::{BlockhashLifetime, Error, ErrorKind, Lamport};

use super::{Client, Context};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureStatus {
    slot: u64,
    failed: bool,
}

impl SignatureStatus {
    #[must_use]
    pub const fn slot(&self) -> u64 {
        self.slot
    }

    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }
}

#[derive(Deserialize)]
struct ContextWire<T> {
    context: SlotWire,
    value: T,
}

#[derive(Deserialize)]
struct SlotWire {
    slot: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifetimeWire {
    blockhash: String,
    last_valid_block_height: u64,
}

#[derive(Deserialize)]
struct SimulationWire {
    err: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusWire {
    slot: u64,
    confirmations: Option<u64>,
    err: Option<serde_json::Value>,
    confirmation_status: String,
}

impl<C> Client<C>
where
    C: json_rpc::Client,
{
    pub async fn latest_blockhash(&self, floor: u64) -> Result<Context<BlockhashLifetime>, Error> {
        let wire = self
            .request::<ContextWire<LifetimeWire>>(
                "getLatestBlockhash",
                json!([{"commitment":"confirmed", "minContextSlot":floor}]),
            )
            .await?;
        require_floor(wire.context.slot, floor)?;
        let blockhash =
            Hash::from_str(&wire.value.blockhash).map_err(|_| malformed("getLatestBlockhash"))?;
        if blockhash.to_string() != wire.value.blockhash {
            return Err(malformed("getLatestBlockhash"));
        }
        Ok(Context {
            slot: wire.context.slot,
            value: BlockhashLifetime::new(blockhash, wire.value.last_valid_block_height),
        })
    }

    pub async fn fee_for_message(
        &self,
        message: &[u8],
        floor: u64,
    ) -> Result<Context<Lamport>, Error> {
        let wire = self
            .request::<ContextWire<Option<u64>>>(
                "getFeeForMessage",
                json!([STANDARD.encode(message), {"commitment":"confirmed", "minContextSlot":floor}]),
            )
            .await?;
        require_floor(wire.context.slot, floor)?;
        let fee = wire
            .value
            .ok_or_else(|| Error::new(ErrorKind::MalformedRpc, "Solana fee is unavailable"))?;
        Ok(Context {
            slot: wire.context.slot,
            value: Lamport::from_atomic(fee),
        })
    }

    pub async fn simulate(&self, transaction: &[u8], floor: u64) -> Result<u64, Error> {
        let wire = self
            .request::<ContextWire<SimulationWire>>(
                "simulateTransaction",
                json!([STANDARD.encode(transaction), {
                    "encoding":"base64",
                    "commitment":"confirmed",
                    "sigVerify":true,
                    "replaceRecentBlockhash":false,
                    "minContextSlot":floor
                }]),
            )
            .await?;
        require_floor(wire.context.slot, floor)?;
        if wire.value.err.is_some() {
            return Err(Error::new(
                ErrorKind::Simulation,
                "Solana transaction simulation failed",
            ));
        }
        Ok(wire.context.slot)
    }

    pub async fn block_height(&self, floor: u64) -> Result<u64, Error> {
        self.request(
            "getBlockHeight",
            json!([{"commitment":"confirmed", "minContextSlot":floor}]),
        )
        .await
    }

    pub async fn send_transaction(
        &self,
        transaction: &[u8],
        floor: u64,
        local_id: TransactionId,
    ) -> Result<(), TransactionError> {
        let returned = self
            .request_after_dispatch::<String>(
                "sendTransaction",
                json!([STANDARD.encode(transaction), {
                    "encoding":"base64",
                    "skipPreflight":false,
                    "preflightCommitment":"confirmed",
                    "minContextSlot":floor,
                    "maxRetries":0
                }]),
                local_id.clone(),
            )
            .await?;
        let signature = Signature::from_str(&returned).map_err(|_| unknown(local_id.clone()))?;
        if signature.to_string() != returned || returned != local_id.as_str() {
            return Err(unknown(local_id));
        }
        Ok(())
    }

    pub async fn signature_status(
        &self,
        local_id: &TransactionId,
        floor: u64,
    ) -> Result<Context<Option<SignatureStatus>>, Error> {
        let wire = self
            .request::<ContextWire<Vec<Option<StatusWire>>>>(
                "getSignatureStatuses",
                json!([[local_id.as_str()], {"searchTransactionHistory":true}]),
            )
            .await?;
        require_floor(wire.context.slot, floor)?;
        let [status] = <[Option<StatusWire>; 1]>::try_from(wire.value)
            .map_err(|_| malformed("getSignatureStatuses"))?;
        let status = status.map(|status| {
            if status.slot < floor
                || status.slot > wire.context.slot
                || !matches!(
                    status.confirmation_status.as_str(),
                    "processed" | "confirmed" | "finalized"
                )
                || (status.confirmation_status == "finalized" && status.confirmations.is_some())
            {
                return Err(malformed("getSignatureStatuses"));
            }
            Ok(SignatureStatus {
                slot: status.slot,
                failed: status.err.is_some(),
            })
        });
        Ok(Context {
            slot: wire.context.slot,
            value: status.transpose()?,
        })
    }
}

fn require_floor(slot: u64, floor: u64) -> Result<(), Error> {
    if slot < floor {
        return Err(Error::new(
            ErrorKind::BelowFloor,
            "Solana RPC response is below its requested context floor",
        ));
    }
    Ok(())
}

fn malformed(method: &str) -> Error {
    Error::new(
        ErrorKind::MalformedRpc,
        format!("Solana RPC {method} returned malformed data"),
    )
}

fn unknown(local_id: TransactionId) -> TransactionError {
    TransactionError::new(
        TransactionErrorKind::Unknown,
        "Solana submission outcome is unknown",
    )
    .with_ambiguous_transaction_id(local_id)
}

#[cfg(test)]
mod tests {
    use solana_hash::Hash;
    use solana_signature::Signature;

    use crate::rpc::test_support::Scripted;

    use super::*;

    #[tokio::test]
    async fn reads_lifetime_fee_simulation_and_height_with_exact_floors() {
        let blockhash = Hash::new_from_array([9; 32]).to_string();
        let message = [1, 2, 3];
        let transaction = [4, 5, 6];
        let rpc = Scripted::new([
            (
                "getLatestBlockhash",
                json!([{"commitment":"confirmed", "minContextSlot":7}]),
                json!({"context":{"slot":8},"value":{"blockhash":blockhash,"lastValidBlockHeight":99}}),
            ),
            (
                "getFeeForMessage",
                json!([STANDARD.encode(message), {"commitment":"confirmed", "minContextSlot":8}]),
                json!({"context":{"slot":9},"value":5000}),
            ),
            (
                "simulateTransaction",
                json!([STANDARD.encode(transaction), {"encoding":"base64","commitment":"confirmed","sigVerify":true,"replaceRecentBlockhash":false,"minContextSlot":9}]),
                json!({"context":{"slot":10},"value":{"err":null,"logs":[]}}),
            ),
            (
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":10}]),
                json!(99),
            ),
        ]);
        let client = Client::new(rpc.clone());
        let lifetime = client.latest_blockhash(7).await.expect("lifetime");
        assert_eq!(lifetime.slot, 8);
        assert_eq!(lifetime.value.last_valid_block_height(), 99);
        assert_eq!(
            client
                .fee_for_message(&message, 8)
                .await
                .unwrap()
                .value
                .atomic(),
            5000
        );
        assert_eq!(client.simulate(&transaction, 9).await.unwrap(), 10);
        assert_eq!(client.block_height(10).await.unwrap(), 99);
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn rejects_null_fee_failed_simulation_and_low_context() {
        let null_fee = Client::new(Scripted::one(
            "getFeeForMessage",
            json!([STANDARD.encode([]), {"commitment":"confirmed", "minContextSlot":4}]),
            json!({"context":{"slot":4},"value":null}),
        ));
        assert_eq!(
            null_fee.fee_for_message(&[], 4).await.unwrap_err().kind(),
            ErrorKind::MalformedRpc
        );
        let failed = Client::new(Scripted::one(
            "simulateTransaction",
            json!([STANDARD.encode([]), {"encoding":"base64","commitment":"confirmed","sigVerify":true,"replaceRecentBlockhash":false,"minContextSlot":4}]),
            json!({"context":{"slot":4},"value":{"err":{"InstructionError":[0,"x"]}}}),
        ));
        assert_eq!(
            failed.simulate(&[], 4).await.unwrap_err().kind(),
            ErrorKind::Simulation
        );
        let low = Client::new(Scripted::one(
            "getLatestBlockhash",
            json!([{"commitment":"confirmed", "minContextSlot":4}]),
            json!({"context":{"slot":3},"value":{"blockhash":Hash::new_from_array([1;32]).to_string(),"lastValidBlockHeight":9}}),
        ));
        assert_eq!(
            low.latest_blockhash(4).await.unwrap_err().kind(),
            ErrorKind::BelowFloor
        );
    }

    #[tokio::test]
    async fn sends_exact_bytes_and_accepts_only_the_local_signature() {
        let signature = Signature::from([7; 64]).to_string();
        let local = TransactionId::new(signature.clone());
        let bytes = [8, 9];
        let rpc = Scripted::one(
            "sendTransaction",
            json!([STANDARD.encode(bytes), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":11,"maxRetries":0}]),
            json!(signature),
        );
        Client::new(rpc.clone())
            .send_transaction(&bytes, 11, local)
            .await
            .expect("matching signature");
        rpc.assert_finished();

        let local = TransactionId::new(Signature::from([7; 64]).to_string());
        let mismatch = Client::new(Scripted::one(
            "sendTransaction",
            json!([STANDARD.encode(bytes), {"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","minContextSlot":11,"maxRetries":0}]),
            json!(Signature::from([8; 64]).to_string()),
        ))
        .send_transaction(&bytes, 11, local.clone())
        .await
        .expect_err("provider mismatch is ambiguous");
        assert_eq!(mismatch.ambiguous_transaction_id, Some(local));
    }

    #[tokio::test]
    async fn reads_exactly_one_coherent_historical_status() {
        let id = TransactionId::new(Signature::from([7; 64]).to_string());
        let params = json!([[id.as_str()], {"searchTransactionHistory":true}]);
        let rpc = Scripted::new([
            (
                "getSignatureStatuses",
                params.clone(),
                json!({"context":{"slot":15},"value":[null]}),
            ),
            (
                "getSignatureStatuses",
                params,
                json!({"context":{"slot":15},"value":[{"slot":12,"confirmations":null,"err":{"x":1},"confirmationStatus":"finalized"}]}),
            ),
        ]);
        let client = Client::new(rpc.clone());
        assert!(
            client
                .signature_status(&id, 10)
                .await
                .unwrap()
                .value
                .is_none()
        );
        let status = client
            .signature_status(&id, 10)
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(status.slot(), 12);
        assert!(status.failed());
        rpc.assert_finished();
    }
}
