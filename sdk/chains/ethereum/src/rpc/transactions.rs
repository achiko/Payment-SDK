use alloy_primitives::keccak256;
use indexing::{BoxFuture, SourceError};
use serde_json::{Map, Value, json};

use super::{
    Client, Limits,
    accounts::{AccountClient, Accounts},
    blocks::{Methods, ProductionClient},
    error::BuildError,
    transport::Client as Transport,
    wire::{
        CallError, address_hex, data_hex, gas_limit_with_margin, invalid_rpc_response,
        is_already_known, map_json_rpc_error, parse_quantity_wei, parse_transaction_id,
        source_error, wei_quantity,
    },
};
use crate::{BuildContext, SignedTransaction, TransactionId, TransferRequest};

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
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
    ) -> BoxFuture<'a, Result<BuildContext, SourceError>>;

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, SourceError>>;
}

impl<C> Transactions for Methods<C>
where
    C: Transport,
{
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
    ) -> BoxFuture<'a, Result<BuildContext, SourceError>> {
        Box::pin(async move {
            if request.data.len() > self.limits()?.max_input_bytes() {
                return Err(source_error(
                    "Ethereum transaction input exceeds the configured size limit",
                    false,
                ));
            }
            if request.to.is_none() && request.data.is_empty() {
                return Err(source_error(
                    "Ethereum contract creation requires non-empty init code",
                    false,
                ));
            }

            self.verify_chain_id().await?;
            let nonce = self.nonce(request.from.clone()).await?;
            let mut transaction = Map::new();
            transaction.insert("from".to_owned(), json!(address_hex(&request.from)));
            if let Some(to) = &request.to {
                transaction.insert("to".to_owned(), json!(address_hex(to)));
            }
            transaction.insert("value".to_owned(), json!(wei_quantity(&request.value)));
            transaction.insert("data".to_owned(), json!(data_hex(&request.data)));

            let estimated_gas_limit = self
                .rpc_u64("eth_estimateGas", json!([Value::Object(transaction)]))
                .await?;
            if estimated_gas_limit == 0 {
                return Err(invalid_rpc_response(
                    "eth_estimateGas",
                    "estimated gas limit is zero",
                ));
            }
            let gas_limit = gas_limit_with_margin(
                estimated_gas_limit,
                self.limits()?.gas_limit_margin_basis_points(),
            )?;
            if gas_limit > self.limits()?.max_gas_limit() {
                return Err(source_error(
                    "Ethereum estimated gas limit exceeds the configured ceiling",
                    false,
                ));
            }

            let max_priority_fee_per_gas =
                self.rpc_wei("eth_maxPriorityFeePerGas", json!([])).await?;
            if &max_priority_fee_per_gas > self.limits()?.max_priority_fee_per_gas() {
                return Err(source_error(
                    "Ethereum priority fee exceeds the configured ceiling",
                    false,
                ));
            }
            let latest_block = self
                .request_result("eth_getBlockByNumber", json!(["latest", false]))
                .await?;
            let latest_block: Value = latest_block.deserialize().map_err(map_json_rpc_error)?;
            let base_fee = latest_block
                .get("baseFeePerGas")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid_rpc_response(
                        "eth_getBlockByNumber",
                        "latest block has no EIP-1559 baseFeePerGas",
                    )
                })
                .and_then(|value| {
                    parse_quantity_wei(value)
                        .map_err(|message| invalid_rpc_response("eth_getBlockByNumber", message))
                })?;
            let max_fee_per_gas = base_fee
                .checked_mul_u64(2)
                .and_then(|fee| fee.checked_add(&max_priority_fee_per_gas))
                .ok_or_else(|| {
                    invalid_rpc_response("eth_getBlockByNumber", "fee calculation overflowed U256")
                })?;
            if &max_fee_per_gas > self.limits()?.max_fee_per_gas() {
                return Err(source_error(
                    "Ethereum max fee per gas exceeds the configured ceiling",
                    false,
                ));
            }
            if max_fee_per_gas < max_priority_fee_per_gas {
                return Err(invalid_rpc_response(
                    "eth_getBlockByNumber",
                    "calculated max fee is below the priority fee",
                ));
            }
            let total_fee = max_fee_per_gas.checked_mul_u64(gas_limit).ok_or_else(|| {
                invalid_rpc_response("fee calculation", "total fee overflowed U256")
            })?;
            if &total_fee > self.limits()?.max_total_fee() {
                return Err(source_error(
                    "Ethereum maximum transaction fee exceeds the configured ceiling",
                    false,
                ));
            }

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
    ) -> BoxFuture<'a, Result<TransactionId, SourceError>> {
        Box::pin(async move {
            let computed = TransactionId(keccak256(&transaction.envelope).0);
            if computed != transaction.id {
                return Err(source_error(
                    "signed Ethereum envelope hash does not match its transaction ID",
                    false,
                ));
            }
            let result = self
                .request_result_detailed(
                    "eth_sendRawTransaction",
                    json!([data_hex(&transaction.envelope)]),
                )
                .await;
            let raw = match result {
                Ok(raw) => raw,
                Err(CallError::Remote(failure)) if is_already_known(&failure) => {
                    if self.confirm_known_transaction(&computed).await? {
                        return Ok(computed);
                    }
                    return Err(source_error(
                        "Ethereum RPC reported an already-known transaction but did not expose the matching hash",
                        true,
                    ));
                }
                Err(error) => return Err(error.into_source("eth_sendRawTransaction")),
            };
            let returned: String = raw.deserialize().map_err(map_json_rpc_error)?;
            let returned = parse_transaction_id(&returned, "eth_sendRawTransaction")?;
            if returned != computed {
                return Err(invalid_rpc_response(
                    "eth_sendRawTransaction",
                    "node hash differs from the locally computed transaction hash",
                ));
            }
            Ok(returned)
        })
    }
}

impl<C> Transactions for TransactionClient<C>
where
    C: Transport,
{
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
    ) -> BoxFuture<'a, Result<BuildContext, SourceError>> {
        self.methods.build_context(request)
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, SourceError>> {
        self.methods.broadcast(transaction)
    }
}
