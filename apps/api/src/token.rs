use std::sync::Arc;

use base::{Address, SignedTransaction};
use deposits::{Collection, CollectionLeg, CollectionLegKind, DepositError};
use wallets::{AddressText, PreparedFee, Wallet};

use crate::collection::{invalid, transaction_error, wallet_error};

/// Resolves the application-owned wallet that funds native transaction fees.
///
/// The application decides which treasury wallet belongs to a configured
/// chain and network. The collection workflow sees only an already-composed
/// wallet and never receives its key material or provider configuration.
pub trait GasWallet: Send + Sync {
    fn wallet<'a>(
        &'a self,
        collection: &'a Collection,
    ) -> wallets::FutureResult<'a, Arc<dyn Wallet>>;
}

pub(super) async fn prepare(
    collection: &Collection,
    leg: &CollectionLeg,
    deposit_wallet: Arc<dyn Wallet>,
    gas_wallet: Option<&dyn GasWallet>,
) -> Result<(Arc<dyn Wallet>, SignedTransaction, Option<base::Decimal>), DepositError> {
    if leg.kind == CollectionLegKind::Sweep {
        let destination = destination(&*deposit_wallet, &collection.destination.value)?;
        let prepared = deposit_wallet
            .sweep(destination)
            .await
            .map_err(wallet_error)?;
        let PreparedFee::Limit(limit) = prepared.fee else {
            return Err(invalid("token wallet returned an exact UTXO fee"));
        };
        return Ok((deposit_wallet, prepared.transaction, Some(limit)));
    }
    let wallet = gas_wallet
        .ok_or_else(|| invalid("token collection has no gas-funding wallet"))?
        .wallet(collection)
        .await
        .map_err(wallet_error)?;
    let destination = deposit_wallet.address();
    let atomic = leg
        .planned_amount
        .as_ref()
        .ok_or_else(|| invalid("gas-funding leg has no planned amount"))?;
    let amount = wallet.display_amount(atomic).map_err(wallet_error)?;
    let mut builder = wallet.transaction();
    builder
        .transfer(destination, amount)
        .map_err(transaction_error)?;
    let transaction = builder.prepare().await.map_err(transaction_error)?;
    Ok((wallet, transaction, None))
}

fn destination(wallet: &dyn Wallet, value: &str) -> Result<Address, DepositError> {
    let encoding = wallet
        .address_text(&wallet.address())
        .map_err(wallet_error)?
        .encoding;
    wallet
        .parse_address(&AddressText::new(encoding, value))
        .map_err(wallet_error)
}
