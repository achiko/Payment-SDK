//! Throughput benchmark for the PostgreSQL indexing backend.
//!
//! Commits synthetic blocks through the real `Blocks::add` path, then reads
//! pages back through `Transactions::list` and `Outputs::list`, so the numbers
//! cover exactly what an indexer and an API do in production.
//!
//!   cargo run --release -p indexing-postgres --example bench -- <url>
//!
//! Shape is environment-driven so variants need no recompile:
//!
//!   BENCH_BLOCKS      blocks to commit                       default 200
//!   BENCH_TXS         transactions per block                 default 40
//!   BENCH_MOVEMENTS   movements per transaction              default 2
//!   BENCH_ADDRESSES   distinct watched addresses             default 16
//!   BENCH_CREATED     outputs created per block              default 40
//!   BENCH_SPENT       outputs spent per block                default 30
//!   BENCH_SCOPE       network label for an intentional rerun default unique
//!   BENCH_RESET       clear only the exact run scope (1/0)    default 1

#[path = "bench/cleanup.rs"]
mod cleanup;

use std::{
    env,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, Blocks,
    CanonicalAddress, ChainId, Decimal, HistoryQuery, IndexScope, IndexedOutput, InterpretedBlock,
    MovementId, ObservationDraft, ObservationDraftStatus, OutputChanges, OutputId, OutputKey,
    OutputRequest, Outputs, TransactionRef, Transactions, ValueMovement,
};

const RETENTION: u64 = 100;

struct Profile {
    blocks: u64,
    txs: usize,
    movements: usize,
    addresses: usize,
    created: usize,
    spent: usize,
}

fn number(key: &str, fallback: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

impl Profile {
    fn from_env() -> Self {
        Self {
            blocks: number("BENCH_BLOCKS", 200) as u64,
            txs: number("BENCH_TXS", 40),
            movements: number("BENCH_MOVEMENTS", 2),
            addresses: number("BENCH_ADDRESSES", 16).max(2),
            created: number("BENCH_CREATED", 40),
            spent: number("BENCH_SPENT", 30),
        }
    }

    /// Rows one block writes, so throughput can be reported per row as well as
    /// per block. Every transaction is stored once per address it touches.
    fn rows(&self) -> usize {
        let addresses_per_tx = 2;
        let history = self.txs * addresses_per_tx;
        let movements = history * self.movements;
        history + movements + self.created + self.spent * 2
    }

    fn address(&self, scope: &IndexScope, index: usize) -> CanonicalAddress {
        let index = index % self.addresses;
        CanonicalAddress {
            scope: scope.clone(),
            value: format!("addr-{index:04}"),
        }
    }
}

fn scope() -> IndexScope {
    let network = env::var("BENCH_SCOPE").unwrap_or_else(|_| {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        format!("run-{}-{stamp}", std::process::id())
    });
    IndexScope {
        chain: ChainId("benchmark".into()),
        network,
    }
}

fn transaction(scope: &IndexScope, height: u64, index: usize) -> TransactionRef {
    TransactionRef {
        scope: scope.clone(),
        value: format!("{height:010}-{index:05}"),
    }
}

/// One block's worth of interpreted facts, spending outputs handed in from the
/// previous block so the spend path is exercised the way a real chain does.
fn interpret(
    scope: &IndexScope,
    profile: &Profile,
    height: u64,
    parent: Option<&BlockRef>,
    spend: Vec<OutputKey>,
) -> (InterpretedBlock, Vec<OutputKey>) {
    let block = BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(format!("hash-{height:010}").into_bytes()),
        parent: parent.map(|block| BlockParent {
            position: block.position,
            hash: block.hash.clone(),
        }),
        timestamp: Some(1_700_000_000 + height),
    };
    let asset = AssetId {
        chain: scope.chain.clone(),
        asset: "native".into(),
    };
    let mut transactions = Vec::with_capacity(profile.txs);
    for index in 0..profile.txs {
        let from = profile.address(scope, index);
        let to = profile.address(scope, index + 1);
        let movements = (0..profile.movements)
            .map(|ordinal| ValueMovement::Transfer {
                id: MovementId(format!("{height}-{index}-{ordinal}")),
                asset: asset.clone(),
                amount: Decimal::from(1_000 + ordinal as u64),
                from: from.clone(),
                to: to.clone(),
            })
            .collect();
        transactions.push(ObservationDraft {
            scope: scope.clone(),
            transaction_id: transaction(scope, height, index),
            status: ObservationDraftStatus::Included,
            movements,
            fee: None,
        });
    }

    let mut created = Vec::with_capacity(profile.created);
    for index in 0..profile.created {
        let owner = profile.address(scope, index);
        created.push(IndexedOutput {
            id: OutputId {
                transaction: transaction(scope, height, index),
                index: index as u32,
            },
            address: owner,
            asset: asset.clone(),
            amount: Decimal::from(50_000 + index as u64),
            // A P2WPKH script is 22 bytes; a P2WSH witness script is larger.
            evidence: vec![0x51; 22],
            created_at: BlockHeight(height),
            coinbase: index == 0,
        });
    }
    let next_spend = created
        .iter()
        .take(profile.spent)
        .map(IndexedOutput::key)
        .collect();

    (
        InterpretedBlock {
            block,
            transactions,
            outputs: OutputChanges {
                created,
                spent: spend,
                tracked_spends: Vec::new(),
            },
        },
        next_spend,
    )
}

fn rate(count: f64, seconds: f64) -> f64 {
    if seconds <= 0.0 { 0.0 } else { count / seconds }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "postgres://prop@127.0.0.1:5433/integration-test".to_owned());
    let profile = Profile::from_env();
    let scope = scope();
    let pool = indexing_postgres::pool(&url, 8)?;

    if env::var("BENCH_RESET").unwrap_or_else(|_| "1".into()) != "0" {
        let mut client = pool.get().await?;
        cleanup::clear_scope(&mut client, &scope).await?;
    }

    let repository = indexing_postgres::Repository::new(pool, scope.clone())?;

    println!(
        "commit: {} blocks x {} tx x {} movements + {} created / {} spent  (~{} rows/block)",
        profile.blocks,
        profile.txs,
        profile.movements,
        profile.created,
        profile.spent,
        profile.rows(),
    );

    // ------------------------------------------------------------ commit path
    let mut parent: Option<BlockRef> = None;
    let mut spend: Vec<OutputKey> = Vec::new();
    let mut worst = 0.0_f64;
    let started = Instant::now();
    for height in 1..=profile.blocks {
        let (interpreted, next_spend) = interpret(&scope, &profile, height, parent.as_ref(), spend);
        let block = interpreted.block.clone();
        let addition = BlockAddition::new(scope.clone(), parent.clone(), RETENTION, interpreted)?;
        let one = Instant::now();
        repository.add(addition).await?;
        worst = worst.max(one.elapsed().as_secs_f64());
        parent = Some(block);
        spend = next_spend;
    }
    let commit = started.elapsed().as_secs_f64();
    println!(
        "  commit  {commit:8.3}s  {:8.1} blocks/s  {:9.0} rows/s  worst block {:.1}ms",
        rate(profile.blocks as f64, commit),
        rate((profile.blocks as usize * profile.rows()) as f64, commit),
        worst * 1000.0,
    );

    // -------------------------------------------------------------- read path
    let pages = 50;
    let started = Instant::now();
    let mut transactions = 0;
    for index in 0..pages {
        let page = Transactions::list(
            &repository,
            HistoryQuery {
                scope: scope.clone(),
                address: profile.address(&scope, index),
                after: None,
                limit: 100,
            },
        )
        .await?;
        transactions += page.transactions.len();
    }
    let history = started.elapsed().as_secs_f64();
    println!(
        "  history {history:8.3}s  {:8.1} pages/s  {:8.2}ms/page  ({transactions} tx read)",
        rate(pages as f64, history),
        history * 1000.0 / pages as f64,
    );

    let started = Instant::now();
    let mut listed = 0;
    for index in 0..pages {
        let page = Outputs::list(
            &repository,
            OutputRequest {
                scope: scope.clone(),
                address: profile.address(&scope, index),
                after: None,
                limit: 100,
            },
        )
        .await?;
        listed += page.outputs.len();
    }
    let outputs = started.elapsed().as_secs_f64();
    println!(
        "  outputs {outputs:8.3}s  {:8.1} pages/s  {:8.2}ms/page  ({listed} outputs read)",
        rate(pages as f64, outputs),
        outputs * 1000.0 / pages as f64,
    );

    Ok(())
}
