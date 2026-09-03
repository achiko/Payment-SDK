use serde_json::{json, value::RawValue};

use crate::{Error, ErrorKind};

use super::Client;

/// Maximum numeric slot span accepted by one finalized enumeration call.
pub(crate) const MAX_ENUMERATION_SPAN: u64 = 500_000;

impl<C> Client<C>
where
    C: json_rpc::Client,
{
    pub(crate) async fn first_available_block(&self) -> Result<u64, Error> {
        self.request("getFirstAvailableBlock", json!([])).await
    }

    pub(crate) async fn finalized_blocks(
        &self,
        start: u64,
        end: u64,
        floor: u64,
    ) -> Result<Vec<u64>, Error> {
        let span = end
            .checked_sub(start)
            .and_then(|difference| difference.checked_add(1))
            .ok_or_else(|| invalid_range("Solana finalized block range must be ordered"))?;
        if span > MAX_ENUMERATION_SPAN {
            return Err(invalid_range(
                "Solana finalized block range exceeds 500000 slots",
            ));
        }

        let slots = self
            .request::<Vec<u64>>(
                "getBlocks",
                json!([start, end, {
                    "commitment": "finalized",
                    "minContextSlot": floor,
                }]),
            )
            .await?;
        validate_slots(&slots, start, end)?;
        Ok(slots)
    }

    pub(crate) async fn finalized_block(&self, slot: u64) -> Result<Option<Box<RawValue>>, Error> {
        self.request(
            "getBlock",
            json!([slot, {
                "commitment": "finalized",
                "encoding": "json",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0,
                "rewards": false,
            }]),
        )
        .await
    }
}

fn validate_slots(slots: &[u64], start: u64, end: u64) -> Result<(), Error> {
    if slots.iter().any(|slot| *slot < start || *slot > end)
        || slots.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(Error::new(
            ErrorKind::MalformedRpc,
            "Solana getBlocks returned unordered, duplicate, or out-of-range slots",
        ));
    }
    Ok(())
}

fn invalid_range(message: &'static str) -> Error {
    Error::new(ErrorKind::InvalidRpcConfiguration, message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::rpc::test_support::Scripted;

    #[tokio::test]
    async fn uses_exact_finalized_indexing_shapes() {
        let raw_block = json!({
            "blockhash": "11111111111111111111111111111111",
            "previousBlockhash": "11111111111111111111111111111111",
            "parentSlot": 6,
            "transactions": [],
            "blockTime": 1,
            "blockHeight": 4,
        });
        let rpc = Scripted::new([
            ("getFirstAvailableBlock", json!([]), json!(2)),
            (
                "getBlocks",
                json!([2, 7, {
                    "commitment": "finalized",
                    "minContextSlot": 9,
                }]),
                json!([2, 4, 7]),
            ),
            (
                "getBlock",
                json!([7, {
                    "commitment": "finalized",
                    "encoding": "json",
                    "transactionDetails": "full",
                    "maxSupportedTransactionVersion": 0,
                    "rewards": false,
                }]),
                raw_block.clone(),
            ),
        ]);
        let client = Client::new(rpc.clone());

        assert_eq!(client.first_available_block().await.unwrap(), 2);
        assert_eq!(client.finalized_blocks(2, 7, 9).await.unwrap(), [2, 4, 7]);
        let block = client.finalized_block(7).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(block.get()).unwrap(),
            raw_block
        );
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn rejects_invalid_request_bounds_before_rpc() {
        let client = Client::new(Scripted::new([]));
        for (start, end) in [(2, 1), (0, MAX_ENUMERATION_SPAN)] {
            assert_eq!(
                client
                    .finalized_blocks(start, end, 9)
                    .await
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidRpcConfiguration
            );
        }
    }

    #[tokio::test]
    async fn rejects_unordered_duplicate_and_out_of_range_slots() {
        for slots in [json!([3, 2]), json!([2, 2]), json!([1]), json!([4])] {
            let client = Client::new(Scripted::one(
                "getBlocks",
                json!([2, 3, {
                    "commitment": "finalized",
                    "minContextSlot": 9,
                }]),
                slots,
            ));
            assert_eq!(
                client.finalized_blocks(2, 3, 9).await.unwrap_err().kind(),
                ErrorKind::MalformedRpc
            );
        }
    }
}
