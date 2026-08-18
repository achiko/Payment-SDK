use base::{Address as BaseAddress, Decimal, TransactionEnvelope, TransactionId};
use wallets::{
    CollectionFactory, Error as WalletError, ErrorKind, FutureResult, PreparedCollection,
    PreparedFee, Sweeper,
};

use super::{PREPARED_KIND, Wallet, configured_chain_id, wallet_error};
use crate::{Address, AssetKind, TransactionBuilder, TransferRequest, Wei};

impl CollectionFactory for Wallet {}

impl Sweeper for Wallet {
    fn sweep<'a>(&'a self, destination: BaseAddress) -> FutureResult<'a, PreparedCollection> {
        Box::pin(async move {
            let destination = ethereum_address(&destination)?;
            let balance = self
                .accounts
                .balance(self.address.clone(), &self.config.asset, None)
                .await
                .map_err(|error| wallet_error(ErrorKind::Balance, error))?;
            if balance.is_zero() {
                return Err(WalletError::new(
                    ErrorKind::InvalidAmount,
                    "Ethereum wallet has no asset balance to sweep",
                ));
            }

            let estimate = estimate_request(self, destination.clone(), balance.clone());
            let context = self
                .transactions
                .build_context(&estimate)
                .await
                .map_err(|error| wallet_error(ErrorKind::Transaction, error))?;
            let expected = configured_chain_id(&self.config.scope)
                .map_err(|error| wallet_error(ErrorKind::Transaction, error))?;
            if context.chain_id != expected {
                return Err(WalletError::new(
                    ErrorKind::Transaction,
                    "Ethereum RPC chain ID does not match the wallet network",
                ));
            }
            let fee = context
                .max_fee_per_gas
                .checked_mul_u64(context.gas_limit)
                .ok_or_else(|| {
                    WalletError::new(
                        ErrorKind::InvalidAmount,
                        "Ethereum maximum sweep fee exceeds the amount range",
                    )
                })?;
            let request = sweep_request(self, destination, balance, &fee).await?;
            let signed = TransactionBuilder::new(request, context)
                .sign(self.signer.as_ref())
                .await
                .map_err(|error| wallet_error(ErrorKind::Transaction, error))?;
            Ok(PreparedCollection {
                transaction: base::SignedTransaction::new(
                    PREPARED_KIND,
                    TransactionId::new(signed.id.to_string()),
                    TransactionEnvelope::new(signed.envelope),
                ),
                fee: PreparedFee::Limit(Decimal::from_atomic(
                    num_bigint::BigUint::from_bytes_be(&fee.0),
                    0,
                )),
            })
        })
    }
}

fn estimate_request(wallet: &Wallet, destination: Address, balance: Wei) -> TransferRequest {
    match &wallet.config.asset {
        AssetKind::Native => {
            TransferRequest::native_atomic(wallet.address.clone(), destination, Wei::ZERO)
        }
        AssetKind::Erc20(token) => {
            TransferRequest::erc20(wallet.address.clone(), token.clone(), destination, balance)
        }
    }
}

async fn sweep_request(
    wallet: &Wallet,
    destination: Address,
    balance: Wei,
    fee: &Wei,
) -> Result<TransferRequest, WalletError> {
    match &wallet.config.asset {
        AssetKind::Native => {
            let value = balance.checked_sub(fee).ok_or_else(insufficient_native)?;
            if value.is_zero() {
                return Err(insufficient_native());
            }
            Ok(TransferRequest::native_atomic(
                wallet.address.clone(),
                destination,
                value,
            ))
        }
        AssetKind::Erc20(token) => {
            let native = wallet
                .accounts
                .balance(wallet.address.clone(), &AssetKind::Native, None)
                .await
                .map_err(|error| wallet_error(ErrorKind::Balance, error))?;
            if native < *fee {
                return Err(WalletError::new(
                    ErrorKind::InvalidAmount,
                    "Ethereum native balance cannot pay the maximum token sweep fee",
                ));
            }
            Ok(TransferRequest::erc20(
                wallet.address.clone(),
                token.clone(),
                destination,
                balance,
            ))
        }
    }
}

fn insufficient_native() -> WalletError {
    WalletError::new(
        ErrorKind::InvalidAmount,
        "Ethereum native balance leaves no value after the maximum sweep fee",
    )
}

fn ethereum_address(address: &BaseAddress) -> Result<Address, WalletError> {
    let bytes: [u8; 20] = address.as_bytes().try_into().map_err(|_| {
        WalletError::new(
            ErrorKind::InvalidAddress,
            "Ethereum destination must contain exactly 20 bytes",
        )
    })?;
    Ok(Address(bytes))
}
