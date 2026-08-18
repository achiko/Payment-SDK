use serde::{Deserialize, Serialize};
use utoipa::IntoParams;

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Chain {
    Bitcoin,
    Ethereum,
}

impl Chain {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bitcoin => "bitcoin",
            Self::Ethereum => "ethereum",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWallet {
    pub chain: Chain,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Wallet {
    pub id: String,
    pub chain: Chain,
    pub network: String,
    pub address: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Balance {
    pub amount: String,
    pub observed_height: Option<u64>,
}

#[path = "dto_history.rs"]
mod dto_history;
pub use dto_history::*;

#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryQuery {
    pub cursor: Option<String>,
    #[serde(default = "default_history_limit")]
    #[param(minimum = 1, maximum = 1000, default = 100)]
    pub limit: usize,
}

impl TryFrom<HistoryQuery> for wallets::HistoryRequest {
    type Error = crate::Error;

    fn try_from(query: HistoryQuery) -> Result<Self, Self::Error> {
        if query.limit == 0 || query.limit > 1_000 {
            return Err(invalid_request("history limit must be between 1 and 1000"));
        }
        let after = query
            .cursor
            .as_deref()
            .map(HistoryCursor::decode)
            .transpose()?;
        Ok(Self {
            after,
            limit: query.limit,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SendFunds {
    pub destination: AddressInput,
    pub amount: String,
}

impl TryFrom<SendFunds> for (wallets::AddressText, base::Decimal) {
    type Error = crate::Error;

    fn try_from(request: SendFunds) -> Result<Self, Self::Error> {
        Ok((request.destination.into(), decimal(&request.amount)?))
    }
}

/// Schema for a chain-native address accepted by transaction endpoints.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddressInput {
    pub encoding: AddressEncoding,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddressEncoding {
    Base58Check,
    Bech32,
    Bech32m,
    Hex,
}

impl From<AddressInput> for wallets::AddressText {
    fn from(address: AddressInput) -> Self {
        let encoding = match address.encoding {
            AddressEncoding::Base58Check => wallets::AddressEncoding::Base58Check,
            AddressEncoding::Bech32 => wallets::AddressEncoding::Bech32,
            AddressEncoding::Bech32m => wallets::AddressEncoding::Bech32m,
            AddressEncoding::Hex => wallets::AddressEncoding::Hex,
        };
        Self::new(encoding, address.text)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Submission {
    pub transaction_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    /// Transfers for one configured chain and network, in execution order.
    pub transfers: Vec<WalletTransfer>,
}

impl TryFrom<TransferRequest> for Vec<crate::WalletSend> {
    type Error = crate::BatchError;

    fn try_from(request: TransferRequest) -> Result<Self, Self::Error> {
        request
            .transfers
            .into_iter()
            .enumerate()
            .map(|(failed_index, transfer)| {
                let amount = decimal(&transfer.amount).map_err(|error| crate::BatchError {
                    transaction_ids: Vec::new(),
                    failed_index,
                    error,
                })?;
                Ok(crate::WalletSend {
                    wallet_id: transfer.wallet_id,
                    destination: transfer.destination.into(),
                    amount,
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WalletTransfer {
    pub wallet_id: String,
    pub destination: AddressInput,
    pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct TransferResponse {
    /// Accepted transaction IDs in chain-native submission order.
    pub transaction_ids: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoryCursor {
    chain: String,
    network: String,
    transaction: String,
}

impl HistoryCursor {
    fn encode(cursor: &indexing::TransactionRef) -> Result<String, crate::Error> {
        use base64::Engine;

        let bytes = serde_json::to_vec(&Self {
            chain: cursor.scope.chain.0.clone(),
            network: cursor.scope.network.clone(),
            transaction: cursor.value.clone(),
        })
        .map_err(|_| invalid_response("history cursor could not be encoded"))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    fn decode(value: &str) -> Result<indexing::TransactionRef, crate::Error> {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| invalid_request("history cursor is invalid"))?;
        let cursor: Self = serde_json::from_slice(&bytes)
            .map_err(|_| invalid_request("history cursor is invalid"))?;
        if cursor.chain.is_empty() || cursor.network.is_empty() || cursor.transaction.is_empty() {
            return Err(invalid_request("history cursor is invalid"));
        }
        Ok(indexing::TransactionRef {
            scope: indexing::IndexScope {
                chain: indexing::ChainId(cursor.chain),
                network: cursor.network,
            },
            value: cursor.transaction,
        })
    }
}

fn decimal(value: &str) -> Result<base::Decimal, crate::Error> {
    value
        .parse()
        .map_err(|_| invalid_request("amount must be an exact base-10 decimal"))
}

fn invalid_request(message: impl Into<String>) -> crate::Error {
    crate::Error::new(crate::ErrorKind::InvalidRequest, message)
}

fn invalid_response(message: impl Into<String>) -> crate::Error {
    crate::Error::new(crate::ErrorKind::InvalidResponse, message)
}

const fn default_history_limit() -> usize {
    100
}

#[cfg(test)]
#[path = "dto_history_test.rs"]
mod history_test;
