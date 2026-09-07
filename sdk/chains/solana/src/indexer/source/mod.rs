//! Bounded sparse-slot acquisition.

mod budget;

use std::{future::Future, time::Duration};

use indexing::{BlockParent, BlockPosition, BlockRef, BlockSource, BoxFuture, SourceError};
use tokio::time::{Instant, timeout_at};

use crate::{Error, ErrorKind, RpcClient, RpcCommitment};

use super::model::Block;

pub use budget::Budget;

const ATTEMPT_DEADLINE: Duration = Duration::from_secs(30);
const DESCENDING_WINDOW: u64 = 10_000;
const MAX_ENUMERATIONS: u8 = 64;
const MAX_FORWARD_WINDOW: u64 = 500_000;

/// Finalized, sparse-slot Solana implementation of the shared block source.
pub struct Source<C> {
    rpc: RpcClient<C>,
}

impl<C> Source<C> {
    #[must_use]
    pub const fn new(rpc: RpcClient<C>) -> Self {
        Self { rpc }
    }
}

impl<C> BlockSource for Source<C>
where
    C: json_rpc::Client,
{
    type Block = Block;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            Attempt::new(&self.rpc)
                .complete_tip()
                .await
                .map(|tip| tip.block.reference().clone())
        })
    }

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        Box::pin(async move {
            if limit == 0 || start > end {
                return Err(source_error(
                    "Solana block range requires ordered positions and a positive limit",
                    false,
                ));
            }
            Attempt::new(&self.rpc)
                .sparse_blocks(start, end, limit)
                .await
        })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        Box::pin(async move {
            Attempt::new(&self.rpc)
                .canonical(position)
                .await
                .map(|block| block.map(|value| value.reference().clone()))
        })
    }
}

struct Attempt<'a, C> {
    rpc: &'a RpcClient<C>,
    deadline: Instant,
    enumerations: u8,
}

struct Tip {
    block: Block,
    lower_bound: u64,
}

impl<'a, C> Attempt<'a, C>
where
    C: json_rpc::Client,
{
    fn new(rpc: &'a RpcClient<C>) -> Self {
        Self {
            rpc,
            deadline: Instant::now() + ATTEMPT_DEADLINE,
            enumerations: 0,
        }
    }

    fn ensure_before_deadline(&self, now: Instant) -> Result<(), SourceError> {
        if now >= self.deadline {
            return Err(source_error(
                "Solana source exceeded its 30-second deadline",
                true,
            ));
        }
        Ok(())
    }

    async fn complete_tip(&mut self) -> Result<Tip, SourceError> {
        let tip = self.finalized_slot().await?;
        let opening = self.first_available().await?;
        if opening > tip {
            return Err(unavailable(
                "Solana first available block is above the finalized slot",
            ));
        }

        let mut end = tip;
        let candidate = loop {
            let start = end.saturating_sub(DESCENDING_WINDOW - 1).max(opening);
            let slots = self.enumerate(start, end, tip).await?;
            if let Some(candidate) = slots.last().copied() {
                break candidate;
            }
            if start == opening {
                return Err(unavailable(
                    "Solana finalized history contains no retained produced block",
                ));
            }
            end = start - 1;
        };
        let block = self.required_block(candidate).await?;
        let closing = self.first_available().await?;
        if closing > candidate {
            return Err(unavailable(
                "Solana selected anchor was pruned during source acquisition",
            ));
        }
        Ok(Tip {
            block,
            lower_bound: closing,
        })
    }

    async fn sparse_blocks(
        &mut self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> Result<Vec<Block>, SourceError> {
        let tip = self.complete_tip().await?;
        if start > tip.block.reference().position {
            return Ok(Vec::new());
        }
        if tip.lower_bound > start.0 {
            return Err(source_error(
                "Solana required position was pruned during source acquisition",
                false,
            ));
        }

        let bounded_end = end.0.min(tip.block.reference().position.0);
        let mut cursor = start.0;
        let mut blocks = Vec::with_capacity(limit.min(64));
        while cursor <= bounded_end && blocks.len() < limit {
            self.ensure_before_deadline(Instant::now())?;
            let remaining = limit - blocks.len();
            let width = u64::try_from(remaining)
                .unwrap_or(u64::MAX)
                .clamp(DESCENDING_WINDOW, MAX_FORWARD_WINDOW);
            let window_end = cursor.saturating_add(width - 1).min(bounded_end);
            let slots = self
                .enumerate(cursor, window_end, tip.block.reference().position.0)
                .await?;
            if window_end == tip.block.reference().position.0
                && slots.last().copied() != Some(window_end)
            {
                return Err(unavailable(
                    "Solana range enumeration omitted its previously proved tip",
                ));
            }

            for slot in slots.into_iter().take(remaining) {
                let block = self.required_block(slot).await?;
                if let Some(previous) = blocks.last() {
                    block.require_connection(previous)?;
                }
                blocks.push(block);
            }
            if blocks.len() == limit || window_end == bounded_end {
                break;
            }
            cursor = window_end
                .checked_add(1)
                .ok_or_else(|| unavailable("Solana slot range cannot advance"))?;
        }

        let closing = self.first_available().await?;
        if closing > start.0 {
            return Err(source_error(
                "Solana required position was pruned during source acquisition",
                false,
            ));
        }
        Ok(blocks)
    }

    async fn canonical(&mut self, position: BlockPosition) -> Result<Option<Block>, SourceError> {
        let tip = self.finalized_slot().await?;
        let opening = self.first_available().await?;
        if opening > position.0 {
            return Err(source_error(
                "Solana required position was pruned during source acquisition",
                false,
            ));
        }
        if position.0 > tip {
            return Err(unavailable(
                "Solana canonical position is above the finalized slot",
            ));
        }

        let slots = self.enumerate(position.0, position.0, tip).await?;
        let block = if slots.is_empty() {
            if tip <= position.0 {
                return Err(unavailable(
                    "Solana same-slot omission is not canonical evidence",
                ));
            }
            None
        } else {
            Some(self.required_block(position.0).await?)
        };
        let closing = self.first_available().await?;
        if closing > position.0 {
            return Err(source_error(
                "Solana required position was pruned during source acquisition",
                false,
            ));
        }
        Ok(block)
    }

    async fn finalized_slot(&self) -> Result<u64, SourceError> {
        within(self.deadline, self.rpc.slot(RpcCommitment::Finalized, None)).await
    }

    async fn first_available(&self) -> Result<u64, SourceError> {
        within(self.deadline, self.rpc.first_available_block()).await
    }

    async fn required_block(&self, slot: u64) -> Result<Block, SourceError> {
        let raw = within(self.deadline, self.rpc.finalized_block(slot))
            .await?
            .ok_or_else(|| unavailable("Solana selected finalized block became unavailable"))?;
        Block::parse(slot, raw.get().as_bytes().to_vec())
            .map_err(|error| source_error(error.to_string(), true))
    }

    async fn enumerate(
        &mut self,
        start: u64,
        end: u64,
        floor: u64,
    ) -> Result<Vec<u64>, SourceError> {
        self.ensure_before_deadline(Instant::now())?;
        if self.enumerations >= MAX_ENUMERATIONS {
            return Err(source_error(
                "Solana source exhausted its 64-call enumeration budget",
                true,
            ));
        }
        self.enumerations += 1;
        within(self.deadline, self.rpc.finalized_blocks(start, end, floor)).await
    }
}

async fn within<T>(
    deadline: Instant,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, SourceError> {
    timeout_at(deadline, future)
        .await
        .map_err(|_| source_error("Solana source exceeded its 30-second deadline", true))?
        .map_err(|error| {
            let retryable = !matches!(
                error.kind(),
                ErrorKind::InvalidRpcConfiguration
                    | ErrorKind::InvalidIdentity
                    | ErrorKind::InvalidBatch
                    | ErrorKind::InvalidBudget
                    | ErrorKind::InvalidSecret
                    | ErrorKind::Generation
                    | ErrorKind::Signing
                    | ErrorKind::UnsupportedDestination
            );
            source_error(error.to_string(), retryable)
        })
}

impl Block {
    fn require_connection(&self, previous: &Self) -> Result<(), SourceError> {
        let previous = previous.reference();
        let current = self.reference();
        let expected_height = previous
            .height
            .checked_successor()
            .ok_or_else(|| unavailable("Solana produced height is exhausted"))?;
        let expected_parent = BlockParent {
            position: previous.position,
            hash: previous.hash.clone(),
        };
        if current.position <= previous.position
            || current.height != expected_height
            || current.parent.as_ref() != Some(&expected_parent)
        {
            return Err(unavailable(
                "Solana produced blocks are not a strict canonical sequence",
            ));
        }
        Ok(())
    }
}

fn unavailable(message: &'static str) -> SourceError {
    source_error(message, true)
}

fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
#[path = "test.rs"]
mod test;
