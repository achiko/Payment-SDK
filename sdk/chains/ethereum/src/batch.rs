use std::sync::Arc;

use base::Address as BaseAddress;
use indexing::SourceError;
use wallets::{Error, ErrorKind, SendError, SendFuture, Sender, Transfer};

use crate::transaction::{Preparation, PreparationError};
use crate::wallet::{WalletConfig, preparation_error};
use crate::{Address, TransactionCoordinator};

pub(crate) struct Batch {
    config: WalletConfig,
    coordinator: Arc<TransactionCoordinator>,
}

impl Batch {
    pub(crate) fn new(config: WalletConfig, coordinator: Arc<TransactionCoordinator>) -> Self {
        Self {
            config,
            coordinator,
        }
    }

    fn preparation<'a>(&self, transfer: &'a Transfer) -> Result<Preparation<'a>, Error> {
        let from = ethereum_address(&transfer.wallet.address())?;
        let destination = transfer.wallet.parse_address(&transfer.to)?;
        let destination = ethereum_address(&destination)?;
        let request = self
            .config
            .transfer_request(from, destination, &transfer.amount)
            .map_err(Error::from)?;
        Ok(Preparation::wallet(
            request,
            self.config.chain_id,
            transfer.wallet.as_ref(),
        ))
    }
}

impl Sender for Batch {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(async move {
            if transfers.is_empty() {
                return Err(failure(0, Vec::new(), "transaction batch is empty"));
            }
            let preparations = transfers
                .iter()
                .enumerate()
                .map(|(index, transfer)| {
                    self.preparation(transfer)
                        .map_err(|error| SendError::at(index, Vec::new(), error))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut prepared = self
                .coordinator
                .prepare_batch(preparations)
                .await
                .map_err(preparation_failure)?;
            let mut accepted = Vec::with_capacity(prepared.len());
            loop {
                let id = prepared.next().await.map_err(|error| {
                    SendError::at(accepted.len(), accepted.clone(), broadcast_error(error))
                })?;
                let Some(id) = id else {
                    return Ok(accepted);
                };
                accepted.push(base::Id::new(id.to_string()));
            }
        })
    }
}

fn ethereum_address(address: &BaseAddress) -> Result<Address, Error> {
    let bytes: [u8; 20] = address.as_bytes().try_into().map_err(|_| {
        Error::new(
            ErrorKind::InvalidAddress,
            "Ethereum address must contain exactly 20 bytes",
        )
    })?;
    Ok(Address(bytes))
}

fn preparation_failure(error: PreparationError) -> SendError {
    SendError::at(
        error.index,
        Vec::new(),
        preparation_error(error.source).into(),
    )
}

fn broadcast_error(error: SourceError) -> Error {
    let kind = if error.retryable {
        ErrorKind::Unavailable
    } else {
        ErrorKind::Transaction
    };
    Error::new(kind, error.message)
}

fn failure(index: usize, accepted: Vec<base::Id>, message: &'static str) -> SendError {
    SendError::at(index, accepted, Error::new(ErrorKind::Transaction, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChainError, ChainErrorKind};

    #[test]
    fn rejects_non_ethereum_source_addresses_before_preparation() {
        let error = ethereum_address(&BaseAddress::new(vec![0_u8; 19]))
            .expect_err("a non-Ethereum address must fail before RPC");

        assert_eq!(error.kind, ErrorKind::InvalidAddress);
    }

    #[test]
    fn preparation_failure_preserves_index_with_zero_accepted() {
        let failure = preparation_failure(PreparationError {
            index: 2,
            source: ChainError {
                kind: ChainErrorKind::InsufficientFunds,
                message: "aggregate balance is insufficient".to_owned(),
            },
        });

        assert_eq!(failure.failed_index, 2);
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.source.kind, ErrorKind::Transaction);
    }

    #[test]
    fn broadcast_failure_preserves_retryability_for_http_mapping() {
        let unavailable = broadcast_error(SourceError {
            message: "submission outcome is ambiguous".to_owned(),
            retryable: true,
        });
        let rejected = broadcast_error(SourceError {
            message: "node rejected the transaction".to_owned(),
            retryable: false,
        });

        assert_eq!(unavailable.kind, ErrorKind::Unavailable);
        assert_eq!(rejected.kind, ErrorKind::Transaction);
    }
}
