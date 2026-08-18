use indexing::{BlockRef, BoxFuture, SourceError};
use serde_json::json;

use super::{
    Client,
    blocks::Methods,
    error::BuildError,
    transport::Client as Transport,
    wire::{
        address_hex, block_parameter, erc20_balance_of_call, invalid_rpc_response,
        map_json_rpc_error, parse_abi_word,
    },
};
use crate::{Address, AssetKind, Wei};

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
                    self.rpc_wei("eth_getBalance", json!([address_hex(&address), block]))
                        .await
                }
                AssetKind::Erc20(token) => {
                    let raw = self
                        .request_result(
                            "eth_call",
                            json!([{
                                "to": address_hex(token),
                                "data": erc20_balance_of_call(&address),
                            }, block]),
                        )
                        .await?;
                    let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
                    parse_abi_word(&value)
                        .map(Wei)
                        .map_err(|message| invalid_rpc_response("eth_call", message))
                }
            }
        })
    }

    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, SourceError>> {
        Box::pin(async move {
            self.rpc_u64(
                "eth_getTransactionCount",
                json!([address_hex(&address), "pending"]),
            )
            .await
        })
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
