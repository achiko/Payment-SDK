use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use indexing::{
    BlockRef, CanonicalAddress, Checkpoint, History, HistoryCursor, HistoryQuery, IndexScope,
};

use crate::RpcClient;

use super::{Envelope, SourceCoordinator};

pub struct Reconciler<C> {
    rpc: RpcClient<C>,
    checkpoint: Arc<dyn Checkpoint>,
    history: Arc<dyn History>,
    scope: IndexScope,
    progress: tokio::sync::watch::Receiver<Option<BlockRef>>,
    sources: SourceCoordinator,
}

pub(super) trait Resolver: Send + Sync {
    fn resolve<'a>(&'a self, envelope: Envelope) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

impl<C> Reconciler<C>
where
    C: json_rpc::Client,
{
    #[must_use]
    pub fn new(
        rpc: RpcClient<C>,
        checkpoint: Arc<dyn Checkpoint>,
        history: Arc<dyn History>,
        scope: IndexScope,
        progress: tokio::sync::watch::Receiver<Option<BlockRef>>,
        sources: SourceCoordinator,
    ) -> Self {
        Self {
            rpc,
            checkpoint,
            history,
            scope,
            progress,
            sources,
        }
    }

    pub async fn resolve(&self, envelope: &Envelope) {
        let mut progress = self.progress.clone();
        let mut previous = None;
        let mut backoff = Backoff::default();
        loop {
            let evidence = self.inspect(envelope).await;
            if matches!(evidence, Evidence::Present | Evidence::Absent) {
                self.sources.release_guard(envelope.source());
                return;
            }
            let checkpoint = evidence.checkpoint().cloned();
            if checkpoint != previous {
                previous = checkpoint;
                backoff.reset();
            }
            let delay = backoff.next();
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                changed = progress.changed() => {
                    if changed.is_ok() && *progress.borrow_and_update() != previous {
                        backoff.reset();
                    }
                }
            }
        }
    }

    async fn inspect(&self, envelope: &Envelope) -> Evidence {
        if self
            .rpc
            .signature_status(envelope.id(), envelope.floor())
            .await
            .ok()
            .is_some_and(|status| status.value.is_some())
        {
            return Evidence::Present;
        }
        let checkpoint = match self.checkpoint.checkpoint(&self.scope).await {
            Ok(Some(checkpoint)) => checkpoint,
            _ => return Evidence::Retry(None),
        };
        match self.scan(envelope, &checkpoint).await {
            Scan::Present => return Evidence::Present,
            Scan::Unstable => return Evidence::Retry(Some(checkpoint)),
            Scan::Exhausted => {}
        }
        let height = match self.rpc.block_height(envelope.floor()).await {
            Ok(height) => height,
            Err(_) => return Evidence::Retry(Some(checkpoint)),
        };
        if height > envelope.lifetime().last_valid_block_height()
            && checkpoint.height.0 >= envelope.lifetime().last_valid_block_height()
        {
            Evidence::Absent
        } else {
            Evidence::Retry(Some(checkpoint))
        }
    }

    async fn scan(&self, envelope: &Envelope, checkpoint: &BlockRef) -> Scan {
        let address = CanonicalAddress {
            scope: self.scope.clone(),
            value: envelope.source().to_string(),
        };
        let mut after: Option<HistoryCursor> = None;
        loop {
            let page = match self
                .history
                .history(HistoryQuery {
                    scope: self.scope.clone(),
                    address: address.clone(),
                    after: after.clone(),
                    limit: 100,
                })
                .await
            {
                Ok(page) if page.checkpoint.as_ref() == Some(checkpoint) => page,
                _ => return Scan::Unstable,
            };
            if page
                .transactions
                .iter()
                .any(|transaction| transaction.transaction_id.value == envelope.id().as_str())
            {
                return Scan::Present;
            }
            match page.next {
                Some(next)
                    if next.checkpoint.as_ref() == Some(checkpoint)
                        && after.as_ref() != Some(&next) =>
                {
                    after = Some(next);
                }
                Some(_) => return Scan::Unstable,
                None => break,
            }
        }
        match self.checkpoint.checkpoint(&self.scope).await {
            Ok(current) if current.as_ref() == Some(checkpoint) => Scan::Exhausted,
            _ => Scan::Unstable,
        }
    }
}

impl<C> Resolver for Reconciler<C>
where
    C: json_rpc::Client,
{
    fn resolve<'a>(&'a self, envelope: Envelope) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move { self.resolve(&envelope).await })
    }
}

enum Evidence {
    Present,
    Absent,
    Retry(Option<BlockRef>),
}

impl Evidence {
    const fn checkpoint(&self) -> Option<&BlockRef> {
        match self {
            Self::Retry(checkpoint) => checkpoint.as_ref(),
            Self::Present | Self::Absent => None,
        }
    }
}

enum Scan {
    Present,
    Exhausted,
    Unstable,
}

struct Backoff(Duration);

impl Default for Backoff {
    fn default() -> Self {
        Self(Duration::from_millis(500))
    }
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let current = self.0;
        self.0 = self.0.saturating_mul(2).min(Duration::from_secs(10));
        current
    }

    fn reset(&mut self) {
        self.0 = Duration::from_millis(500);
    }
}

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockPosition};
    use indexing::{BoxFuture, ChainId, IndexError, TransactionPage};
    use serde_json::json;
    use solana_hash::Hash;

    use crate::{
        Address, BlockhashLifetime, Key, Lamport, Memo, Message, ResolvedTransfer, Seed,
        rpc::test_support::Scripted,
    };

    use super::*;

    struct Index {
        checkpoint: Option<BlockRef>,
        page_checkpoint: Option<BlockRef>,
    }

    impl Checkpoint for Index {
        fn checkpoint<'a>(
            &'a self,
            _scope: &'a IndexScope,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async move { Ok(self.checkpoint.clone()) })
        }
    }

    impl History for Index {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async move {
                Ok(TransactionPage {
                    checkpoint: self.page_checkpoint.clone(),
                    transactions: Vec::new(),
                    next: None,
                })
            })
        }
    }

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "localnet".to_owned(),
        }
    }

    fn checkpoint(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height + 100),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8; 32]),
            parent: None,
            timestamp: None,
        }
    }

    fn envelope() -> (Envelope, Arc<Key>) {
        let key = Arc::new(
            Key::from_seed(
                "0707070707070707070707070707070707070707070707070707070707070707"
                    .parse::<Seed>()
                    .expect("seed"),
            )
            .expect("key"),
        );
        let lifetime = BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let message = Message::native_transfer(
            key.address(),
            &Address::from_bytes([8; 32]),
            Lamport::from_atomic(3),
            Memo::from_bytes([3; Memo::LENGTH]),
            &lifetime,
        )
        .expect("message");
        let envelope = Envelope::sign(key.address().clone(), 0, message, 11, lifetime, &key)
            .expect("envelope");
        (envelope, key)
    }

    #[test]
    fn backoff_doubles_to_ten_seconds_and_resets_on_progress() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.next(), Duration::from_millis(500));
        assert_eq!(backoff.next(), Duration::from_secs(1));
        assert_eq!(backoff.next(), Duration::from_secs(2));
        assert_eq!(backoff.next(), Duration::from_secs(4));
        assert_eq!(backoff.next(), Duration::from_secs(8));
        assert_eq!(backoff.next(), Duration::from_secs(10));
        assert_eq!(backoff.next(), Duration::from_secs(10));
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_millis(500));
    }

    #[tokio::test]
    async fn status_presence_releases_exactly_the_ambiguous_source() {
        let (envelope, key) = envelope();
        let id = envelope.id().clone();
        let rpc = Scripted::one(
            "getSignatureStatuses",
            json!([[id.as_str()], {"searchTransactionHistory":true}]),
            json!({"context":{"slot":15},"value":[{"slot":12,"confirmations":null,"err":null,"confirmationStatus":"finalized"}]}),
        );
        let source = SourceCoordinator::default();
        let transfer = ResolvedTransfer::new(
            0,
            key.address().clone(),
            Address::from_bytes([8; 32]).to_string(),
            Lamport::from_atomic(3),
        );
        source
            .lease(std::slice::from_ref(&transfer), false)
            .expect("lease")
            .guard()
            .retain_ambiguity(key.address());
        let index = Arc::new(Index {
            checkpoint: None,
            page_checkpoint: None,
        });
        let (_progress_send, progress) = tokio::sync::watch::channel(None);
        Reconciler::new(
            RpcClient::new(rpc.clone()),
            index.clone(),
            index,
            scope(),
            progress,
            source.clone(),
        )
        .resolve(&envelope)
        .await;
        source
            .lease(&[transfer], false)
            .expect("presence releases source");
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn terminal_absence_requires_expiry_coverage_and_one_stable_exhausted_scan() {
        let (envelope, _) = envelope();
        let id = envelope.id().clone();
        let rpc = Scripted::new([
            (
                "getSignatureStatuses",
                json!([[id.as_str()], {"searchTransactionHistory":true}]),
                json!({"context":{"slot":15},"value":[null]}),
            ),
            (
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":11}]),
                json!(45),
            ),
        ]);
        let block = checkpoint(44);
        let index = Arc::new(Index {
            checkpoint: Some(block.clone()),
            page_checkpoint: Some(block),
        });
        let (_progress_send, progress) = tokio::sync::watch::channel(None);
        let reconciler = Reconciler::new(
            RpcClient::new(rpc.clone()),
            index.clone(),
            index,
            scope(),
            progress,
            SourceCoordinator::default(),
        );
        assert!(matches!(
            reconciler.inspect(&envelope).await,
            Evidence::Absent
        ));
        rpc.assert_finished();
    }

    #[tokio::test]
    async fn checkpoint_conflict_keeps_absence_unresolved_without_height_rpc() {
        let (envelope, _) = envelope();
        let id = envelope.id().clone();
        let rpc = Scripted::one(
            "getSignatureStatuses",
            json!([[id.as_str()], {"searchTransactionHistory":true}]),
            json!({"context":{"slot":15},"value":[null]}),
        );
        let index = Arc::new(Index {
            checkpoint: Some(checkpoint(44)),
            page_checkpoint: Some(checkpoint(43)),
        });
        let (_progress_send, progress) = tokio::sync::watch::channel(None);
        let reconciler = Reconciler::new(
            RpcClient::new(rpc.clone()),
            index.clone(),
            index,
            scope(),
            progress,
            SourceCoordinator::default(),
        );
        assert!(matches!(
            reconciler.inspect(&envelope).await,
            Evidence::Retry(Some(_))
        ));
        rpc.assert_finished();
    }
}
