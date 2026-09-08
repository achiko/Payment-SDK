use alloy_primitives::{hex, keccak256};
use base::{TransactionError, TransactionErrorKind, TransactionId as BaseTransactionId};
use indexing::{BoxFuture, SourceError};
use serde_json::{Map, Value, json};

use super::{
    Client, Limits,
    accounts::{AccountClient, Accounts},
    blocks::{Methods, ProductionClient},
    error::BuildError,
    transport::Client as Transport,
    wire::{
        CallError, invalid_rpc_response, map_json_rpc_error, parse_fixed_data, parse_quantity_u64,
    },
};
use crate::{
    AssetKind, BuildContext, ChainError, ChainErrorKind, SignedTransaction, TransactionId,
    TransferRequest, Wei, erc20,
};

pub type HttpAccounts = AccountClient<ProductionClient>;
pub type HttpTransactions = TransactionClient<ProductionClient>;

/// Focused transaction calls over a shared RPC client.
pub struct TransactionClient<C> {
    methods: Methods<C>,
}

impl<C> TransactionClient<C> {
    pub fn new(
        client: Client<C>,
        expected_chain_id: u64,
        limits: Limits,
    ) -> Result<Self, BuildError> {
        Methods::from_client(client, expected_chain_id, Some(limits))
            .map(|methods| Self { methods })
    }

    pub(super) fn from_methods(methods: Methods<C>) -> Self {
        Self { methods }
    }
}

/// Transaction preparation and submission.
pub trait Transactions: Send + Sync {
    /// Preflights a transfer using the nonce reserved by the caller.
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
        nonce: u64,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>>;

    /// Submits one exact signed envelope and verifies the returned hash.
    ///
    /// Implementations must use one visible submission attempt. A definitive
    /// pre-wire or pre-acceptance rejection stays ID-free. Transport failures
    /// and malformed, missing, or mismatched responses after attempting the
    /// envelope must carry only the ID derived from those exact local bytes.
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, TransactionError>>;

    /// Reports whether the node exposes the exact requested transaction hash.
    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
    ) -> BoxFuture<'a, Result<bool, SourceError>>;
}

impl<C> Transactions for Methods<C>
where
    C: Transport,
{
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
        nonce: u64,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
        Box::pin(async move {
            let input = request.input();
            let limits = self.limits().map_err(ChainError::from_rpc)?;
            if input.len() > limits.max_input_bytes() {
                return Err(ChainError::new(
                    ChainErrorKind::InvalidTransaction,
                    "Ethereum transaction input exceeds the configured size limit",
                ));
            }

            self.verify_transaction_chain_id().await?;
            let mut transaction = Map::new();
            transaction.insert("from".to_owned(), json!(request.from().to_string()));
            transaction.insert("to".to_owned(), json!(request.to().to_string()));
            transaction.insert("value".to_owned(), json!(request.value().to_quantity()));
            transaction.insert("data".to_owned(), json!(hex::encode_prefixed(&input)));

            if request.erc20_transfer().is_some() {
                self.ensure_token_amount(request).await?;
                self.simulate_token_transfer(Value::Object(transaction.clone()))
                    .await?;
            }

            let estimated_gas_limit = self.estimate_gas(Value::Object(transaction)).await?;
            if estimated_gas_limit == 0 {
                return Err(ChainError::from_rpc(invalid_rpc_response(
                    "eth_estimateGas",
                    "estimated gas limit is zero",
                )));
            }
            let gas_limit = limits
                .gas_limit_with_margin(estimated_gas_limit)
                .map_err(ChainError::from_rpc)?;
            if gas_limit > limits.max_gas_limit() {
                return Err(ChainError::new(
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum estimated gas limit exceeds the configured ceiling",
                ));
            }

            let max_priority_fee_per_gas = self
                .rpc_wei("eth_maxPriorityFeePerGas", json!([]))
                .await
                .map_err(ChainError::from_rpc)?;
            if &max_priority_fee_per_gas > limits.max_priority_fee_per_gas() {
                return Err(ChainError::new(
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum priority fee exceeds the configured ceiling",
                ));
            }
            let latest_block = self
                .request_result("eth_getBlockByNumber", json!(["latest", false]))
                .await
                .map_err(ChainError::from_rpc)?;
            let latest_block: Value = latest_block
                .deserialize()
                .map_err(map_json_rpc_error)
                .map_err(ChainError::from_rpc)?;
            let base_fee = latest_block
                .get("baseFeePerGas")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ChainError::from_rpc(invalid_rpc_response(
                        "eth_getBlockByNumber",
                        "latest block has no EIP-1559 baseFeePerGas",
                    ))
                })?;
            let base_fee = Wei::from_quantity(base_fee)
                .map_err(|message| invalid_rpc_response("eth_getBlockByNumber", message))
                .map_err(ChainError::from_rpc)?;
            let max_fee_per_gas = base_fee
                .checked_mul_u64(2)
                .and_then(|fee| fee.checked_add(&max_priority_fee_per_gas))
                .ok_or_else(|| {
                    ChainError::from_rpc(invalid_rpc_response(
                        "eth_getBlockByNumber",
                        "fee calculation overflowed U256",
                    ))
                })?;
            if &max_fee_per_gas > limits.max_fee_per_gas() {
                return Err(ChainError::new(
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum max fee per gas exceeds the configured ceiling",
                ));
            }
            if max_fee_per_gas < max_priority_fee_per_gas {
                return Err(ChainError::from_rpc(invalid_rpc_response(
                    "eth_getBlockByNumber",
                    "calculated max fee is below the priority fee",
                )));
            }
            let total_fee = max_fee_per_gas.checked_mul_u64(gas_limit).ok_or_else(|| {
                ChainError::new(
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum maximum transaction fee overflowed U256",
                )
            })?;
            if &total_fee > limits.max_total_fee() {
                return Err(ChainError::new(
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum maximum transaction fee exceeds the configured ceiling",
                ));
            }

            self.ensure_native_funds(request, &total_fee).await?;

            Ok(BuildContext {
                chain_id: self.expected_chain_id,
                nonce,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            })
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, TransactionError>> {
        Box::pin(async move {
            let computed = TransactionId(keccak256(&transaction.envelope).0);
            if computed != transaction.id {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidTransaction,
                    "signed Ethereum envelope hash does not match its transaction ID",
                ));
            }
            let result = self
                .request_result_detailed_once(
                    "eth_sendRawTransaction",
                    json!([hex::encode_prefixed(&transaction.envelope)]),
                )
                .await;
            let raw = match result {
                Ok(raw) => Some(raw),
                Err(error) if error.is_already_known() => None,
                Err(CallError::Remote(failure)) => {
                    return Err(ambiguous_submission(
                        &computed,
                        CallError::Remote(failure).into_source("eth_sendRawTransaction"),
                    ));
                }
                Err(CallError::Local(error)) => {
                    return Err(ambiguous_submission(&computed, error));
                }
            };
            let Some(raw) = raw else {
                return match self.confirm_known_transaction(&computed).await {
                    Ok(true) => Ok(computed),
                    Ok(false) => Err(ambiguous_submission(
                        &computed,
                        "Ethereum RPC reported an already-known transaction but did not expose the matching hash",
                    )),
                    Err(error) => Err(ambiguous_submission(&computed, error)),
                };
            };
            let returned: String = raw
                .deserialize()
                .map_err(map_json_rpc_error)
                .map_err(|error| ambiguous_submission(&computed, error))?;
            let returned = TransactionId::from_rpc(&returned, "eth_sendRawTransaction")
                .map_err(|error| ambiguous_submission(&computed, error))?;
            if returned != computed {
                return Err(ambiguous_submission(
                    &computed,
                    "Ethereum submission response hash differs from the exact signed envelope; outcome is ambiguous",
                ));
            }
            Ok(returned)
        })
    }

    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
    ) -> BoxFuture<'a, Result<bool, SourceError>> {
        Box::pin(async move { self.confirm_known_transaction(transaction).await })
    }
}

impl<C> Methods<C>
where
    C: Transport,
{
    async fn verify_transaction_chain_id(&self) -> Result<(), ChainError> {
        let actual = self
            .rpc_u64("eth_chainId", json!([]))
            .await
            .map_err(ChainError::from_rpc)?;
        if actual != self.expected_chain_id {
            return Err(ChainError::new(
                ChainErrorKind::Divergent,
                format!(
                    "Ethereum RPC chain ID {actual} does not match configured chain ID {}",
                    self.expected_chain_id
                ),
            ));
        }
        Ok(())
    }

    async fn estimate_gas(&self, transaction: Value) -> Result<u64, ChainError> {
        let raw = match self
            .request_result_detailed("eth_estimateGas", json!([transaction]))
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                if let Some(code) = error.execution_revert_code() {
                    return Err(ChainError::new(
                        ChainErrorKind::Rejected,
                        format!("Ethereum gas estimation was rejected with code {code}"),
                    ));
                }
                return Err(ChainError::from_rpc(error.into_source("eth_estimateGas")));
            }
        };
        let value: String = raw
            .deserialize()
            .map_err(map_json_rpc_error)
            .map_err(ChainError::from_rpc)?;
        parse_quantity_u64(&value)
            .map_err(|message| invalid_rpc_response("eth_estimateGas", message))
            .map_err(ChainError::from_rpc)
    }

    async fn simulate_token_transfer(&self, transaction: Value) -> Result<(), ChainError> {
        let raw = match self
            .request_result_detailed("eth_call", json!([transaction, "pending"]))
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                if let Some(code) = error.execution_revert_code() {
                    return Err(ChainError::new(
                        ChainErrorKind::Rejected,
                        format!(
                            "Ethereum ERC-20 transfer simulation was rejected with code {code}"
                        ),
                    ));
                }
                return Err(ChainError::from_rpc(error.into_source("eth_call")));
            }
        };
        let value: String = raw.deserialize().map_err(|_| {
            ChainError::new(
                ChainErrorKind::Rejected,
                "Ethereum ERC-20 transfer simulation returned an invalid JSON value",
            )
        })?;
        let word = parse_fixed_data::<32>(&value, "ERC-20 transfer result").map_err(|message| {
            ChainError::new(
                ChainErrorKind::Rejected,
                format!("Ethereum ERC-20 transfer simulation returned invalid data: {message}"),
            )
        })?;
        let transferred = erc20::decode_transfer(&word).map_err(|_| {
            ChainError::new(
                ChainErrorKind::Rejected,
                "Ethereum ERC-20 transfer simulation returned an invalid ABI result",
            )
        })?;
        if !transferred {
            return Err(ChainError::new(
                ChainErrorKind::Rejected,
                "Ethereum ERC-20 transfer simulation returned false",
            ));
        }
        Ok(())
    }

    async fn ensure_token_amount(&self, request: &TransferRequest) -> Result<(), ChainError> {
        let Some((token, amount)) = request.erc20_transfer() else {
            return Ok(());
        };
        let token_asset = AssetKind::Erc20(token.clone());
        let token_balance = self
            .balance(request.from().clone(), &token_asset, None)
            .await
            .map_err(ChainError::from_rpc)?;
        if token_balance < *amount {
            return Err(ChainError::new(
                ChainErrorKind::InsufficientFunds,
                "Ethereum ERC-20 balance is insufficient for the transfer amount",
            ));
        }
        Ok(())
    }

    async fn ensure_native_funds(
        &self,
        request: &TransferRequest,
        total_fee: &crate::Wei,
    ) -> Result<(), ChainError> {
        let native = AssetKind::Native;
        let native_balance = self
            .balance(request.from().clone(), &native, None)
            .await
            .map_err(ChainError::from_rpc)?;
        if request.erc20_transfer().is_none() {
            let required = request.value().checked_add(total_fee).ok_or_else(|| {
                ChainError::new(
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum transfer value plus maximum fee overflowed U256",
                )
            })?;
            if native_balance < required {
                return Err(ChainError::new(
                    ChainErrorKind::InsufficientFunds,
                    "Ethereum native balance is insufficient for transfer value and maximum fee",
                ));
            }
            return Ok(());
        }

        if native_balance < *total_fee {
            return Err(ChainError::new(
                ChainErrorKind::InsufficientFunds,
                "Ethereum native balance is insufficient for the ERC-20 maximum fee",
            ));
        }
        Ok(())
    }
}

impl<C> Transactions for TransactionClient<C>
where
    C: Transport,
{
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
        nonce: u64,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
        self.methods.build_context(request, nonce)
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, TransactionError>> {
        self.methods.broadcast(transaction)
    }

    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
    ) -> BoxFuture<'a, Result<bool, SourceError>> {
        self.methods.known(transaction)
    }
}

// design-lint: allow unclassified-free-function -- RPC boundary adapter adds Ethereum ambiguity context and the exact local transaction ID to uncertain submission failures without client state
fn ambiguous_submission(id: &TransactionId, error: impl std::fmt::Display) -> TransactionError {
    TransactionError::new(
        TransactionErrorKind::Unavailable,
        format!("Ethereum submission outcome is ambiguous: {error}"),
    )
    .with_ambiguous_transaction_id(BaseTransactionId::new(id.to_string()))
}
