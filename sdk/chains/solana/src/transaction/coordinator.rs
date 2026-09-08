use std::{collections::BTreeMap, sync::Arc};

use indexing::{BlockRef, Checkpoint, History, IndexScope};
use wallets::{Error as WalletError, ErrorKind as WalletErrorKind, SendError, SendFuture};

use crate::{Batch, NativeSender, NativeTransfer, RpcClient};

use super::{
    Acquirer, Cancellation, Preparer, Reconciler, ResolvedTransfer, SourceCoordinator,
    SubmissionRegistrar, Submitter,
};

/// One source-keyed path for complete native SOL preparation and submission.
pub struct Coordinator<C> {
    acquirer: Acquirer<C>,
    preparer: Preparer<C>,
    submitter: Submitter<C>,
}

impl<C> Coordinator<C>
where
    C: json_rpc::Client + 'static,
{
    #[must_use]
    pub fn new(
        rpc: RpcClient<C>,
        registrar: Arc<dyn SubmissionRegistrar>,
        checkpoint: Arc<dyn Checkpoint>,
        history: Arc<dyn History>,
        scope: IndexScope,
        progress: tokio::sync::watch::Receiver<Option<BlockRef>>,
    ) -> Self {
        let sources = SourceCoordinator::default();
        let reconciler = Reconciler::new(
            rpc.clone(),
            checkpoint,
            history,
            scope,
            progress,
            sources.clone(),
        );
        Self {
            acquirer: Acquirer::new(rpc.clone(), sources),
            preparer: Preparer::new(rpc.clone()),
            submitter: Submitter::new(rpc, registrar, reconciler),
        }
    }

    async fn send(
        &self,
        transfers: Vec<NativeTransfer>,
        cancellation: &Cancellation,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        let mut keys = BTreeMap::new();
        let resolved = transfers
            .into_iter()
            .enumerate()
            .map(|(index, transfer)| {
                let source = transfer.source().clone();
                let signer = transfer.signer();
                if signer.address() != &source {
                    return Err(SendError::item(
                        index,
                        Vec::new(),
                        WalletError::new(
                            WalletErrorKind::AddressMismatch,
                            "Solana signer does not own the source address",
                        ),
                    ));
                }
                keys.entry(source.clone()).or_insert(signer);
                Ok(ResolvedTransfer::new(
                    index,
                    source,
                    transfer.destination().to_string(),
                    transfer.amount(),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = Batch::new(resolved).map_err(|error| {
            SendError::collection(WalletErrorKind::InvalidBatch, error.to_string())
        })?;
        let acquired = self.acquirer.acquire(batch, cancellation).await?;
        let prepared = self.preparer.prepare(acquired, &keys, cancellation).await?;
        self.submitter.submit(prepared, cancellation).await
    }
}

impl<C> NativeSender for Coordinator<C>
where
    C: json_rpc::Client + 'static,
{
    fn send<'a>(&'a self, transfers: Vec<NativeTransfer>) -> SendFuture<'a> {
        Box::pin(async move { self.send(transfers, &Cancellation::default()).await })
    }
}
