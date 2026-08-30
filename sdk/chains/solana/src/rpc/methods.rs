use std::{fmt, str::FromStr};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::json;
use solana_hash::Hash;

use crate::{AccountSnapshot, Address, Error, ErrorKind, Lamport};

use super::Client;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commitment {
    Confirmed,
    Finalized,
}

impl Commitment {
    const fn text(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Finalized => "finalized",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenesisHash(Hash);

impl FromStr for GenesisHash {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let hash = Hash::from_str(input).map_err(|_| {
            Error::new(
                ErrorKind::InvalidIdentity,
                "Solana genesis hash must be canonical Base58",
            )
        })?;
        if hash.to_string() != input {
            return Err(Error::new(
                ErrorKind::InvalidIdentity,
                "Solana genesis hash must be canonical Base58",
            ));
        }
        Ok(Self(hash))
    }
}

impl fmt::Display for GenesisHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context<T> {
    pub slot: u64,
    pub value: T,
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
struct AccountWire {
    lamports: u64,
    owner: String,
    executable: bool,
    data: serde_json::Value,
    space: u64,
}

impl<C> Client<C>
where
    C: json_rpc::Client,
{
    pub async fn health(&self) -> Result<(), Error> {
        let health = self.request::<String>("getHealth", json!([])).await?;
        if health != "ok" {
            return Err(Error::new(
                ErrorKind::MalformedRpc,
                "Solana RPC health result is not exactly ok",
            ));
        }
        Ok(())
    }

    pub async fn genesis_hash(&self) -> Result<GenesisHash, Error> {
        let text = self.request::<String>("getGenesisHash", json!([])).await?;
        text.parse().map_err(|_| malformed("getGenesisHash"))
    }

    pub async fn slot(&self, commitment: Commitment, minimum: Option<u64>) -> Result<u64, Error> {
        let params = match minimum {
            Some(floor) => json!([{ "commitment": commitment.text(), "minContextSlot": floor }]),
            None => json!([{ "commitment": commitment.text() }]),
        };
        let slot = self.request::<u64>("getSlot", params).await?;
        require_floor(slot, minimum)?;
        Ok(slot)
    }

    pub async fn account(
        &self,
        address: &Address,
        commitment: Commitment,
        minimum: Option<u64>,
    ) -> Result<Context<Option<AccountSnapshot>>, Error> {
        let config = account_config(commitment, minimum);
        let wire = self
            .request::<ContextWire<Option<AccountWire>>>(
                "getAccountInfo",
                json!([address.to_string(), config]),
            )
            .await?;
        require_floor(wire.context.slot, minimum)?;
        Ok(Context {
            slot: wire.context.slot,
            value: wire.value.map(AccountSnapshot::try_from).transpose()?,
        })
    }

    pub async fn accounts(
        &self,
        addresses: &[Address],
        commitment: Commitment,
        minimum: Option<u64>,
    ) -> Result<Context<Vec<Option<AccountSnapshot>>>, Error> {
        if addresses.is_empty() || addresses.len() > 100 {
            return Err(Error::new(
                ErrorKind::InvalidRpcConfiguration,
                "Solana multi-account reads require between 1 and 100 addresses",
            ));
        }
        let config = account_config(commitment, minimum);
        let texts = addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let wire = self
            .request::<ContextWire<Vec<Option<AccountWire>>>>(
                "getMultipleAccounts",
                json!([texts, config]),
            )
            .await?;
        require_floor(wire.context.slot, minimum)?;
        if wire.value.len() != addresses.len() {
            return Err(malformed("getMultipleAccounts"));
        }
        let values = wire
            .value
            .into_iter()
            .map(|account| account.map(AccountSnapshot::try_from).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Context {
            slot: wire.context.slot,
            value: values,
        })
    }

    pub async fn balance(
        &self,
        address: &Address,
        minimum: Option<u64>,
    ) -> Result<Context<Lamport>, Error> {
        let params = match minimum {
            Some(floor) => {
                json!([address.to_string(), {"commitment":"finalized", "minContextSlot":floor}])
            }
            None => json!([address.to_string(), {"commitment":"finalized"}]),
        };
        let wire = self
            .request::<ContextWire<u64>>("getBalance", params)
            .await?;
        require_floor(wire.context.slot, minimum)?;
        Ok(Context {
            slot: wire.context.slot,
            value: Lamport::from_atomic(wire.value),
        })
    }
}

impl TryFrom<AccountWire> for AccountSnapshot {
    type Error = Error;

    fn try_from(wire: AccountWire) -> Result<Self, Self::Error> {
        let owner = wire
            .owner
            .parse::<Address>()
            .map_err(|_| malformed("account"))?;
        let tuple = wire.data.as_array().ok_or_else(|| malformed("account"))?;
        if tuple.len() != 2 || tuple[1].as_str() != Some("base64") {
            return Err(malformed("account"));
        }
        let encoded = tuple[0].as_str().ok_or_else(|| malformed("account"))?;
        let data = STANDARD.decode(encoded).map_err(|_| malformed("account"))?;
        if STANDARD.encode(&data) != encoded || u64::try_from(data.len()) != Ok(wire.space) {
            return Err(malformed("account"));
        }
        Ok(AccountSnapshot::new(
            owner,
            Lamport::from_atomic(wire.lamports),
            wire.executable,
            data,
        ))
    }
}

fn account_config(commitment: Commitment, minimum: Option<u64>) -> serde_json::Value {
    match minimum {
        Some(floor) => {
            json!({"encoding":"base64", "commitment":commitment.text(), "minContextSlot":floor})
        }
        None => json!({"encoding":"base64", "commitment":commitment.text()}),
    }
}

fn require_floor(value: u64, floor: Option<u64>) -> Result<(), Error> {
    if floor.is_some_and(|floor| value < floor) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::test_support::Scripted;

    fn account(owner: &Address, lamports: u64, data: &str, space: u64) -> serde_json::Value {
        json!({
            "lamports":lamports,
            "owner":owner.to_string(),
            "executable":false,
            "data":[data,"base64"],
            "space":space
        })
    }

    #[tokio::test]
    async fn reads_identity_and_contextual_slots_with_exact_shapes() {
        let hash = Hash::new_from_array([9; 32]).to_string();
        let rpc = Scripted::new([
            ("getHealth", json!([]), json!("ok")),
            ("getGenesisHash", json!([]), json!(hash.clone())),
            (
                "getSlot",
                json!([{"commitment":"confirmed"}]),
                json!(u64::MAX),
            ),
            (
                "getSlot",
                json!([{"commitment":"finalized", "minContextSlot":8}]),
                json!(8),
            ),
        ]);
        let client = Client::new(rpc.clone());
        client.health().await.expect("health");
        assert_eq!(client.genesis_hash().await.unwrap().to_string(), hash);
        assert_eq!(
            client.slot(Commitment::Confirmed, None).await.unwrap(),
            u64::MAX
        );
        assert_eq!(
            client.slot(Commitment::Finalized, Some(8)).await.unwrap(),
            8
        );
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn rejects_non_ok_health_malformed_hash_number_forms_and_low_floor() {
        for value in [json!("behind"), json!(true), json!(null)] {
            let client = Client::new(Scripted::one("getHealth", json!([]), value));
            assert!(client.health().await.is_err());
        }
        let client = Client::new(Scripted::one("getGenesisHash", json!([]), json!("bad")));
        assert_eq!(
            client.genesis_hash().await.unwrap_err().kind(),
            ErrorKind::MalformedRpc
        );
        for value in [json!(-1), json!(1.5), json!("9")] {
            let client = Client::new(Scripted::one(
                "getSlot",
                json!([{"commitment":"confirmed"}]),
                value,
            ));
            assert_eq!(
                client
                    .slot(Commitment::Confirmed, None)
                    .await
                    .unwrap_err()
                    .kind(),
                ErrorKind::MalformedRpc
            );
        }
        let client = Client::new(Scripted::one(
            "getSlot",
            json!([{"commitment":"confirmed", "minContextSlot":9}]),
            json!(8),
        ));
        assert_eq!(
            client
                .slot(Commitment::Confirmed, Some(9))
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::BelowFloor
        );
    }

    #[tokio::test]
    async fn decodes_complete_accounts_and_finalized_balances() {
        let address = Address::from_bytes([7; 32]);
        let owner = Address::from_bytes([0; 32]);
        let rpc = Scripted::new([
            (
                "getAccountInfo",
                json!([address.to_string(), {"encoding":"base64", "commitment":"confirmed", "minContextSlot":4}]),
                json!({"context":{"slot":5,"apiVersion":"x"},"value":account(&owner,9,"AQI=",2)}),
            ),
            (
                "getMultipleAccounts",
                json!([[address.to_string(),owner.to_string()], {"encoding":"base64", "commitment":"confirmed", "minContextSlot":5}]),
                json!({"context":{"slot":6},"value":[null,account(&owner,u64::MAX,"",0)]}),
            ),
            (
                "getBalance",
                json!([address.to_string(), {"commitment":"finalized", "minContextSlot":6}]),
                json!({"context":{"slot":6},"value":u64::MAX}),
            ),
        ]);
        let client = Client::new(rpc.clone());
        let one = client
            .account(&address, Commitment::Confirmed, Some(4))
            .await
            .unwrap();
        assert_eq!(one.slot, 5);
        assert_eq!(one.value.unwrap().data(), &[1, 2]);
        let many = client
            .accounts(&[address.clone(), owner], Commitment::Confirmed, Some(5))
            .await
            .unwrap();
        assert_eq!(many.slot, 6);
        assert!(many.value[0].is_none());
        assert_eq!(
            many.value[1].as_ref().unwrap().lamports().atomic(),
            u64::MAX
        );
        let balance = client.balance(&address, Some(6)).await.unwrap();
        assert_eq!(balance.value.atomic(), u64::MAX);
        assert_eq!(balance.value.decimal().to_string(), "18446744073.709551615");
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn rejects_multi_bounds_cardinality_and_every_account_encoding_violation() {
        let address = Address::from_bytes([7; 32]);
        let owner = Address::from_bytes([0; 32]);
        let client = Client::new(Scripted::new([]));
        assert!(
            client
                .accounts(&[], Commitment::Confirmed, None)
                .await
                .is_err()
        );
        assert!(
            client
                .accounts(&vec![address.clone(); 101], Commitment::Confirmed, None)
                .await
                .is_err()
        );

        for value in [
            json!({"context":{"slot":1},"value":[]}),
            json!({"context":{"slot":1},"value":[account(&owner,1,"AQI=",1)]}),
            json!({"context":{"slot":1},"value":[account(&owner,1,"AQ",1)]}),
            json!({"context":{"slot":1},"value":[{"lamports":1,"owner":"bad","executable":false,"data":["","base64"],"space":0}]}),
            json!({"context":{"slot":1},"value":[{"lamports":1,"owner":owner.to_string(),"executable":false,"data":["","base58"],"space":0}]}),
        ] {
            let client = Client::new(Scripted::one(
                "getMultipleAccounts",
                json!([[address.to_string()], {"encoding":"base64", "commitment":"confirmed"}]),
                value,
            ));
            assert_eq!(
                client
                    .accounts(std::slice::from_ref(&address), Commitment::Confirmed, None)
                    .await
                    .unwrap_err()
                    .kind(),
                ErrorKind::MalformedRpc
            );
        }
    }
}
