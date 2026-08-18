use wallets::{Error, ErrorKind, SendError, SendFuture, Sender as BatchSender, Transfer};

pub(crate) enum Batch {
    Sequential,
}

impl BatchSender for Batch {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(async move {
            if transfers.is_empty() {
                return Err(failure(0, Vec::new(), "transaction batch is empty"));
            }
            let count = transfers.len();
            let mut prepared = Vec::with_capacity(count);
            for (index, transfer) in transfers.into_iter().enumerate() {
                let destination = transfer
                    .wallet
                    .parse_address(&transfer.to)
                    .map_err(|error| SendError::at(index, Vec::new(), error))?;
                let mut transaction = transfer.wallet.transaction();
                transaction
                    .transfer(destination, transfer.amount)
                    .map_err(|error| SendError::at(index, Vec::new(), transaction_error(error)))?;
                prepared.push((transfer.wallet, transaction));
            }

            let mut ids = Vec::with_capacity(count);
            for (index, (wallet, mut transaction)) in prepared.into_iter().enumerate() {
                let signed = transaction
                    .prepare()
                    .await
                    .map_err(|error| SendError::at(index, ids.clone(), transaction_error(error)))?;
                let submitted = wallet
                    .broadcaster()
                    .broadcast(&signed)
                    .await
                    .map_err(|error| SendError::at(index, ids.clone(), transaction_error(error)))?;
                ids.push(submitted.id);
            }
            Ok(ids)
        })
    }
}

fn failure(index: usize, accepted: Vec<base::Id>, message: &'static str) -> SendError {
    SendError::at(index, accepted, Error::new(ErrorKind::Transaction, message))
}

fn transaction_error(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Transaction, error.to_string())
}
