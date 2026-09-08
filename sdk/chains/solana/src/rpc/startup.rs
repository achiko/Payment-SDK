use crate::{Address, Error, ErrorKind};

use super::{Client, Commitment, GenesisHash};

impl<C> Client<C>
where
    C: json_rpc::Client,
{
    /// Verifies the configured cluster identity with one endpoint-affine call.
    pub async fn verify_genesis(&self, expected: &GenesisHash) -> Result<(), Error> {
        if &self.genesis_hash().await? != expected {
            return Err(Error::new(
                ErrorKind::InvalidIdentity,
                "Solana RPC genesis hash does not match configuration",
            ));
        }
        Ok(())
    }

    /// Requires the exact Memo-v3 program to be executable at one finalized
    /// contextual floor before storage is opened.
    pub async fn verify_memo(&self) -> Result<(), Error> {
        let floor = self.slot(Commitment::Finalized, None).await?;
        let address = Address::from_bytes(spl_memo_interface::v3::ID.to_bytes());
        let account = self
            .account(&address, Commitment::Finalized, Some(floor))
            .await?
            .value
            .ok_or_else(missing_memo)?;
        if !account.executable() {
            return Err(missing_memo());
        }
        Ok(())
    }
}

fn missing_memo() -> Error {
    Error::new(
        ErrorKind::InvalidIdentity,
        "Solana Memo-v3 program is absent or not executable",
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::rpc::test_support::Scripted;

    const ZERO: &str = "11111111111111111111111111111111";
    const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

    #[tokio::test]
    async fn verifies_genesis_and_exact_finalized_executable_memo() {
        let account = account(true, 41, json!(["", "base64"]));
        let script = Scripted::new([
            ("getGenesisHash", json!([]), json!(ZERO)),
            ("getSlot", json!([{"commitment":"finalized"}]), json!(41)),
            (
                "getAccountInfo",
                json!([MEMO, {
                    "encoding":"base64",
                    "commitment":"finalized",
                    "minContextSlot":41
                }]),
                account,
            ),
        ]);
        let client = Client::new(script.clone());

        client
            .verify_genesis(&ZERO.parse().expect("canonical genesis"))
            .await
            .expect("matching genesis");
        client.verify_memo().await.expect("executable Memo-v3");
        script.assert_finished();
    }

    #[tokio::test]
    async fn wrong_genesis_stops_before_the_memo_probe() {
        let script = Scripted::one("getGenesisHash", json!([]), json!(ZERO));
        let client = Client::new(script.clone());
        let expected = solana_hash::Hash::new_from_array([7; 32]).to_string();

        assert_eq!(
            client
                .verify_genesis(&expected.parse().expect("canonical genesis"))
                .await
                .expect_err("wrong genesis")
                .kind(),
            ErrorKind::InvalidIdentity
        );
        script.assert_finished();
    }

    #[tokio::test]
    async fn rejects_absent_non_executable_malformed_and_below_floor_memo() {
        for (slot, value, kind) in [
            (41, Value::Null, ErrorKind::InvalidIdentity),
            (
                41,
                account(false, 41, json!(["", "base64"]))["value"].clone(),
                ErrorKind::InvalidIdentity,
            ),
            (
                41,
                account(true, 41, json!(["%%%", "base64"]))["value"].clone(),
                ErrorKind::MalformedRpc,
            ),
            (
                40,
                account(true, 40, json!(["", "base64"]))["value"].clone(),
                ErrorKind::BelowFloor,
            ),
        ] {
            let script = Scripted::new([
                ("getSlot", json!([{"commitment":"finalized"}]), json!(41)),
                (
                    "getAccountInfo",
                    json!([MEMO, {
                        "encoding":"base64",
                        "commitment":"finalized",
                        "minContextSlot":41
                    }]),
                    json!({"context":{"slot":slot}, "value":value}),
                ),
            ]);
            let client = Client::new(script.clone());

            assert_eq!(
                client.verify_memo().await.expect_err("invalid Memo").kind(),
                kind
            );
            script.assert_finished();
        }
    }

    fn account(executable: bool, slot: u64, data: Value) -> Value {
        json!({
            "context": {"slot": slot},
            "value": {
                "lamports": 1,
                "owner": ZERO,
                "executable": executable,
                "data": data,
                "space": 0
            }
        })
    }
}
