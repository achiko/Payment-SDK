use base::{
    Address as BaseAddress, Decimal, TransactionBuilder as BaseBuilder, TransactionError,
    TransactionErrorKind, TransactionFuture, TransactionId as BaseTransactionId,
    TransactionSnapshot,
};

use super::provider::{PREPARED_KIND, SNAPSHOT_KIND, Wallet, network_name, transaction_error};
use crate::{Address, BuildRequest, Output, SpendSource, TransactionBuilder, TransactionId};

pub(super) struct Builder {
    scope: indexing::IndexScope,
    network: crate::Network,
    source: Address,
    signer: std::sync::Arc<base::KeyPair<Address>>,
    utxos: std::sync::Arc<crate::IndexUtxos>,
    fees: std::sync::Arc<dyn crate::Fees>,
    fee_target_blocks: u16,
    max_fee_rate: crate::FeeRate,
    recipients: Vec<(Address, Decimal)>,
}

impl Wallet {
    pub(super) fn builder(&self) -> Builder {
        Builder {
            scope: self.config.scope.clone(),
            network: self.config.network,
            source: self.address.clone(),
            signer: self.signer.clone(),
            utxos: self.utxos.clone(),
            fees: self.fees.clone(),
            fee_target_blocks: self.config.fee_target_blocks,
            max_fee_rate: self.config.max_fee_rate,
            recipients: Vec::new(),
        }
    }
}

impl Builder {
    pub(super) fn restore(
        wallet: &Wallet,
        snapshot: &TransactionSnapshot,
    ) -> Result<Self, TransactionError> {
        let mut builder = wallet.builder();
        builder.recipients = super::snapshot::decode(&wallet.config, &wallet.address, snapshot)?;
        builder.validate()?;
        Ok(builder)
    }

    fn validate(&self) -> Result<(), TransactionError> {
        if self.scope.chain.0 != "bitcoin" || self.scope.network != network_name(self.network) {
            return Err(transaction_error(
                TransactionErrorKind::InvalidSnapshot,
                "Bitcoin transaction identity, chain, and network do not agree",
            ));
        }
        Address::parse_for_network(self.source.encoded(), self.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidSnapshot, error))?;
        Ok(())
    }
}

impl BaseBuilder for Builder {
    fn transfer(
        &mut self,
        destination: BaseAddress,
        amount: Decimal,
    ) -> Result<(), TransactionError> {
        let value = std::str::from_utf8(destination.as_bytes()).map_err(|_| {
            transaction_error(
                TransactionErrorKind::InvalidAddress,
                "Bitcoin address is not UTF-8",
            )
        })?;
        let address = Address::parse_for_network(value, self.network)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAddress, error))?;
        crate::Satoshi::from_decimal(&amount)
            .map_err(|error| transaction_error(TransactionErrorKind::InvalidAmount, error))?;
        self.recipients.push((address, amount));
        Ok(())
    }

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        self.validate()?;
        if self.recipients.is_empty() {
            return Err(transaction_error(
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
            ));
        }
        let transfers = self
            .recipients
            .iter()
            .map(|(destination, amount)| {
                serde_json::json!({
                    "destination": destination.to_string(),
                    "amount": amount.to_string(),
                })
            })
            .collect::<Vec<_>>();
        Ok(TransactionSnapshot::new(
            SNAPSHOT_KIND,
            serde_json::json!({
                "scope": {
                    "chain": self.scope.chain.0.as_str(),
                    "network": self.scope.network.as_str(),
                },
                "source": self.source.to_string(),
                "asset": {
                    "kind": "native",
                    "ticker": crate::BTC.ticker,
                    "decimals": crate::BTC.decimals,
                },
                "transfers": transfers,
                "change": self.source.to_string(),
            }),
        ))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            self.validate()?;
            if self.recipients.is_empty() {
                return Err(transaction_error(
                    TransactionErrorKind::InvalidTransaction,
                    "transaction has no recipients",
                ));
            }
            let set = self
                .utxos
                .utxos(vec![self.source.clone()])
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            let available = set
                .outputs
                .into_iter()
                .map(|output| {
                    SpendSource::from_exact_selection(
                        self.network,
                        &self.source,
                        TransactionId(output.transaction_id),
                        output.output_index,
                        output.value,
                        output.script_pubkey,
                    )
                    .map_err(|error| {
                        transaction_error(TransactionErrorKind::InvalidTransaction, error)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let recipients = self
                .recipients
                .iter()
                .cloned()
                .map(|(address, amount)| {
                    Output::new(address, amount).map_err(|error| {
                        transaction_error(TransactionErrorKind::InvalidAmount, error)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let fee_rate = self
                .fees
                .estimate(self.fee_target_blocks)
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Unavailable, error))?;
            if fee_rate > self.max_fee_rate {
                return Err(transaction_error(
                    TransactionErrorKind::Fee,
                    "estimated Bitcoin fee rate exceeds the configured maximum",
                ));
            }
            let request = BuildRequest {
                available,
                recipients,
                change_address: self.source.clone(),
                fee_rate,
                drain_wallet: false,
            };
            let signed = TransactionBuilder::new(self.network, request)
                .sign(self.signer.as_ref())
                .await
                .map_err(|error| transaction_error(TransactionErrorKind::Signing, error))?;
            Ok(base::SignedTransaction::new(
                PREPARED_KIND,
                BaseTransactionId::new(signed.id().to_string()),
                base::TransactionEnvelope::new(signed.consensus_bytes().to_vec()),
            ))
        })
    }
}
