use chain_bitcoin::BlockInterpreter as BitcoinBlockInterpreter;
use chain_ethereum::BlockInterpreter as EthereumBlockInterpreter;
use indexing::{
    AbortRebuild, BeginRebuild, BlockHeight, BlockInterpreter as IndexBlockInterpreter,
    BlockSource, CleanupGeneration, CommitBlock, IndexScope, PrepareActivation, RebuildActivation,
    RebuildAdmin, RebuildBlock, RebuildBuilder, RebuildGeneration, RebuildPhase, RebuildPublisher,
    RebuildReader, RebuildValidation, WatchReader,
};
use storage_rocksdb::RocksDb;

use crate::config::{
    BitcoinGeneration, BitcoinRebuild, EthereumGeneration, EthereumRebuild,
    bitcoin_bootstrap_height, bootstrap_height,
};

use super::{
    AppResult, bitcoin_repository, connect_bitcoin_source, connect_source, failure, repository,
};

pub async fn rebuild(options: EthereumRebuild) -> AppResult<()> {
    options.repository.validate()?;
    options.source.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDb::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = repository(storage, &options.repository)?;
    let source = connect_source(&scope, &options.source).await?;
    let interpreter = EthereumBlockInterpreter::new(scope.clone())?;
    rebuild_runtime(
        repository,
        source,
        interpreter,
        scope,
        bootstrap_height(&options.repository),
        options.repository.confirmation_policy()?,
        options.repository.reorg_retention,
    )
    .await
}

pub async fn rebuild_bitcoin(options: BitcoinRebuild) -> AppResult<()> {
    options.repository.validate()?;
    options.source.validate()?;
    let scope = options.repository.scope()?;
    let network = options.repository.network()?;
    let storage = RocksDb::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    let source = connect_bitcoin_source(&scope, network, &options.source).await?;
    let interpreter = BitcoinBlockInterpreter::new(scope.clone(), network)?;
    rebuild_runtime(
        repository,
        source,
        interpreter,
        scope,
        bitcoin_bootstrap_height(&options.repository),
        options.repository.confirmation_policy()?,
        options.repository.reorg_retention,
    )
    .await
}

async fn rebuild_runtime<S, I, R>(
    repository: R,
    source: S,
    interpreter: I,
    scope: IndexScope,
    bootstrap_height: BlockHeight,
    confirmation_policy: indexing::ConfirmationPolicy,
    reorg_retention: u64,
) -> AppResult<()>
where
    S: BlockSource,
    I: IndexBlockInterpreter<Block = S::Block>,
    R: WatchReader<Target = I::Target, Effect = I::Effect, Undo = I::Undo>
        + RebuildReader
        + RebuildBuilder
        + RebuildPublisher,
{
    let mut state = repository
        .begin_rebuild(BeginRebuild {
            scope: scope.clone(),
            bootstrap_height,
        })
        .await?;
    if state.phase == RebuildPhase::Building {
        let tip = source.tip().await?;
        let mut next = match &state.checkpoint {
            Some(checkpoint) => checkpoint
                .height
                .0
                .checked_add(1)
                .map(BlockHeight)
                .ok_or_else(|| failure("rebuild checkpoint height is exhausted"))?,
            None => bootstrap_height,
        };
        while next <= tip.height {
            let watches = repository.watches_at(&scope, next).await?;
            let block = source.block_at(next).await?;
            let interpreted = interpreter.inspect(&block, &watches.watches)?;
            if source.canonical_hash(next).await?.as_ref() != Some(&interpreted.block.hash) {
                return Err(failure(
                    "canonical block changed during rebuild; leave the generation unpublished and rerun rebuild",
                ));
            }
            repository
                .commit_rebuild_block(RebuildBlock {
                    generation: state.generation,
                    command: CommitBlock {
                        scope: scope.clone(),
                        expected_checkpoint: state.checkpoint.clone(),
                        expected_watch_version: watches.version,
                        confirmation_policy,
                        reorg_retention,
                        block: interpreted,
                    },
                })
                .await?;
            state = repository
                .rebuild_state(&scope)
                .await?
                .ok_or_else(|| failure("rebuild manifest disappeared after block commit"))?;
            next = next
                .0
                .checked_add(1)
                .map(BlockHeight)
                .ok_or_else(|| failure("rebuild height is exhausted"))?;
        }
    }
    let checkpoint = state
        .checkpoint
        .ok_or_else(|| failure("rebuild produced no canonical checkpoint"))?;
    if source.canonical_hash(checkpoint.height).await?.as_ref() != Some(&checkpoint.hash) {
        return Err(failure(
            "staged rebuild checkpoint is no longer canonical; leave the generation unpublished and rerun rebuild",
        ));
    }
    repository
        .validate_rebuild(RebuildValidation {
            scope: scope.clone(),
            generation: state.generation,
            expected_checkpoint: checkpoint.clone(),
        })
        .await?;
    repository
        .prepare_rebuild_activation(PrepareActivation {
            scope: scope.clone(),
            generation: state.generation,
            expected_checkpoint: checkpoint.clone(),
        })
        .await?;
    // The journal preparation may be expensive. Recheck the exact checkpoint
    // immediately before the atomic publication fence.
    if source.canonical_hash(checkpoint.height).await?.as_ref() != Some(&checkpoint.hash) {
        return Err(failure(
            "staged rebuild checkpoint changed while activation was prepared; leave the generation unpublished and rerun rebuild",
        ));
    }
    repository
        .activate_rebuild(RebuildActivation {
            scope,
            generation: state.generation,
            expected_checkpoint: checkpoint,
        })
        .await?;
    tracing::info!(generation = state.generation.0, "staged rebuild activated");
    Ok(())
}

pub async fn abort_rebuild(options: EthereumGeneration) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDb::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = repository(storage, &options.repository)?;
    repository
        .abort_rebuild(AbortRebuild {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(generation = options.generation, "staged rebuild aborted");
    Ok(())
}

pub async fn abort_bitcoin_rebuild(options: BitcoinGeneration) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDb::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    repository
        .abort_rebuild(AbortRebuild {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(
        generation = options.generation,
        "Bitcoin staged rebuild aborted"
    );
    Ok(())
}

pub async fn cleanup(options: EthereumGeneration) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDb::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = repository(storage, &options.repository)?;
    let outcome = repository
        .cleanup_generation(CleanupGeneration {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(generation = options.generation, outcome = ?outcome, "generation cleanup completed");
    Ok(())
}

pub async fn cleanup_bitcoin(options: BitcoinGeneration) -> AppResult<()> {
    options.repository.validate()?;
    let scope = options.repository.scope()?;
    let storage = RocksDb::open(&options.repository.database.database_path)?;
    storage.create_backup(&options.backup_path).await?;
    let repository = bitcoin_repository(storage, &options.repository)?;
    let outcome = repository
        .cleanup_generation(CleanupGeneration {
            scope,
            generation: RebuildGeneration(options.generation),
        })
        .await?;
    tracing::info!(generation = options.generation, outcome = ?outcome, "Bitcoin generation cleanup completed");
    Ok(())
}
