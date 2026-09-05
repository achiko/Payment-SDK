use indexing::{BlockRef, BoxFuture, SourceError};
use serde_json::json;

use super::{
    Client,
    blocks::Methods,
    error::BuildError,
    transport::Client as Transport,
    wire::{
        block_parameter, data_hex, invalid_rpc_response, map_json_rpc_error, parse_data,
        parse_fixed_data, source_error,
    },
};
use crate::{Address, AssetKind, Wei, erc20};

/// Focused account and nonce calls over a shared RPC client.
pub struct AccountClient<C> {
    methods: Methods<C>,
}

impl<C> AccountClient<C> {
    pub fn new(client: Client<C>, expected_chain_id: u64) -> Result<Self, BuildError> {
        Methods::from_client(client, expected_chain_id, None).map(|methods| Self { methods })
    }

    pub(super) fn from_methods(methods: Methods<C>) -> Self {
        Self { methods }
    }
}

impl<C> AccountClient<C>
where
    C: Transport,
{
    /// Validates a configured ERC-20 contract at one canonical block.
    ///
    /// The client must be bound to one already-admitted RPC endpoint. A
    /// failover transport cannot guarantee that chain identity and every
    /// contract probe are answered by the same endpoint.
    pub fn validate_token<'a>(
        &'a self,
        token: &'a Address,
        expected_decimals: u8,
    ) -> BoxFuture<'a, Result<(), SourceError>> {
        Box::pin(self.methods.validate_token(token, expected_decimals))
    }
}

/// Account reads used by wallet balance and transaction preparation.
pub trait Accounts: Send + Sync {
    fn balance<'a>(
        &'a self,
        address: Address,
        asset: &'a AssetKind,
        at: Option<BlockRef>,
    ) -> BoxFuture<'a, Result<Wei, SourceError>>;

    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, SourceError>>;
}

impl<C> Accounts for Methods<C>
where
    C: Transport,
{
    fn balance<'a>(
        &'a self,
        address: Address,
        asset: &'a AssetKind,
        at: Option<BlockRef>,
    ) -> BoxFuture<'a, Result<Wei, SourceError>> {
        Box::pin(async move {
            let block = block_parameter(at)?;
            match asset {
                AssetKind::Native => {
                    self.rpc_wei("eth_getBalance", json!([address.to_string(), block]))
                        .await
                }
                AssetKind::Erc20(token) => {
                    if token.is_zero() {
                        return Err(source_error(
                            "Ethereum ERC-20 token address must not be zero",
                            false,
                        ));
                    }
                    let raw = self
                        .request_result(
                            "eth_call",
                            json!([{
                                "to": token.to_string(),
                                "data": data_hex(&erc20::balance_of(&address)),
                            }, block]),
                        )
                        .await?;
                    let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
                    let word = parse_fixed_data::<32>(&value, "ERC-20 balance result")
                        .map_err(|message| invalid_rpc_response("eth_call", message))?;
                    erc20::decode_balance(&word)
                        .map_err(|_| invalid_rpc_response("eth_call", "invalid balanceOf result"))
                }
            }
        })
    }

    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, SourceError>> {
        Box::pin(async move {
            self.rpc_u64(
                "eth_getTransactionCount",
                json!([address.to_string(), "pending"]),
            )
            .await
        })
    }
}

impl<C> Methods<C>
where
    C: Transport,
{
    async fn validate_token(
        &self,
        token: &Address,
        expected_decimals: u8,
    ) -> Result<(), SourceError> {
        if token.is_zero() {
            return Err(source_error(
                "Ethereum ERC-20 token address must not be zero",
                false,
            ));
        }
        self.verify_chain_id().await?;
        let block = self.latest_canonical_parameter().await?;

        let raw = self
            .request_result("eth_getCode", json!([token.to_string(), block.clone()]))
            .await?;
        let code: String = raw.deserialize().map_err(map_json_rpc_error)?;
        if parse_data(&code)
            .map_err(|message| invalid_rpc_response("eth_getCode", message))?
            .is_empty()
        {
            return Err(source_error(
                "Ethereum ERC-20 token address has no deployed code",
                false,
            ));
        }

        let decimals = self
            .erc20_word(token, erc20::decimals(), block.clone())
            .await
            .and_then(|word| {
                erc20::decode_decimals(&word)
                    .map_err(|_| invalid_rpc_response("eth_call", "invalid ERC-20 decimals result"))
            })?;
        if decimals != expected_decimals {
            return Err(source_error(
                format!(
                    "Ethereum ERC-20 token decimals {decimals} do not match configured decimals {expected_decimals}"
                ),
                false,
            ));
        }

        self.erc20_word(token, erc20::balance_of(&Address([0; 20])), block)
            .await
            .and_then(|word| {
                erc20::decode_balance(&word).map_err(|_| {
                    invalid_rpc_response("eth_call", "invalid ERC-20 balanceOf result")
                })
            })?;
        Ok(())
    }

    async fn erc20_word(
        &self,
        token: &Address,
        input: Vec<u8>,
        block: serde_json::Value,
    ) -> Result<[u8; 32], SourceError> {
        let raw = self
            .request_result(
                "eth_call",
                json!([{
                    "to": token.to_string(),
                    "data": data_hex(&input),
                }, block]),
            )
            .await?;
        let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
        parse_fixed_data(&value, "ERC-20 result")
            .map_err(|message| invalid_rpc_response("eth_call", message))
    }
}

impl<C> Accounts for AccountClient<C>
where
    C: Transport,
{
    fn balance<'a>(
        &'a self,
        address: Address,
        asset: &'a AssetKind,
        at: Option<BlockRef>,
    ) -> BoxFuture<'a, Result<Wei, SourceError>> {
        self.methods.balance(address, asset, at)
    }

    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, SourceError>> {
        self.methods.nonce(address)
    }
}
