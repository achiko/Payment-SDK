//! Throughput benchmark for the embedded redb indexing backend.
//!
//! Deliberately the same workload and the same reporting as
//! `indexing-postgres`'s `bench` example, so the two backends can be compared
//! directly:
//!
//!   cargo run --release -p indexing-redb --example bench
//!
//! Shape is environment-driven and shares the postgres example's variables:
//! BENCH_BLOCKS, BENCH_TXS, BENCH_MOVEMENTS, BENCH_ADDRESSES, BENCH_CREATED,
//! and BENCH_SPENT.

use std::{env, time::Instant};

use futures_executor::block_on;
use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockRef, Blocks, CanonicalAddress, ChainId,
    Decimal, HistoryQuery, IndexScope, IndexedOutput, InterpretedBlock, MovementId,
    ObservationDraft, ObservationDraftStatus, OutputChanges, OutputId, OutputKey, OutputRequest,
    Outputs, TransactionRef, Transactions, ValueMovement,
};
use indexing_redb::Repository;

const RETENTION: u64 = 100;
const CHAIN: &str = "primary";
const NETWORK: &str = "testing";

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

    fn rows(&self) -> usize {
        let addresses_per_tx = 2;
        let history = self.txs * addresses_per_tx;
        let movements = history * self.movements;
        history + movements + self.created + self.spent * 2
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId(CHAIN.into()),
        network: NETWORK.into(),
    }
}

fn address(index: usize) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope(),
        value: format!("addr-{index:04}"),
    }
}

fn transaction(height: u64, index: usize) -> TransactionRef {
    TransactionRef {
        scope: scope(),
        value: format!("{height:010}-{index:05}"),
    }
}

fn asset() -> AssetId {
    AssetId {
        chain: ChainId(CHAIN.into()),
        asset: "native".into(),
    }
}

fn amount(units: u64) -> Decimal {
    units.to_string().parse().expect("amount parses")
}

fn block_ref(height: u64, parent: Option<&BlockRef>) -> BlockRef {
    BlockRef {
        height: BlockHeight(height),
        hash: BlockHash(format!("hash-{height:010}").into_bytes()),
        parent_hash: parent.map(|block| block.hash.clone()),
        timestamp: Some(1_700_000_000 + height),
    }
}

fn interpret(
    profile: &Profile,
    height: u64,
    parent: Option<&BlockRef>,
    spend: Vec<OutputKey>,
) -> (InterpretedBlock, Vec<OutputKey>) {
    let block = block_ref(height, parent);
    let mut transactions = Vec::with_capacity(profile.txs);
    for index in 0..profile.txs {
        let from = address(index % profile.addresses);
        let to = address((index + 1) % profile.addresses);
        let movements = (0..profile.movements)
            .map(|ordinal| ValueMovement::Transfer {
                id: MovementId(format!("{height}-{index}-{ordinal}")),
                asset: asset(),
                amount: amount(1_000 + ordinal as u64),
                from: from.clone(),
                to: to.clone(),
            })
            .collect();
        transactions.push(ObservationDraft {
            scope: scope(),
            transaction_id: transaction(height, index),
            status: ObservationDraftStatus::Included,
            movements,
            fee: None,
        });
    }

    let mut created = Vec::with_capacity(profile.created);
    for index in 0..profile.created {
        created.push(IndexedOutput {
            id: OutputId {
                transaction: transaction(height, index),
                index: index as u32,
            },
            address: address(index % profile.addresses),
            asset: asset(),
            amount: amount(50_000 + index as u64),
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile = Profile::from_env();
    let directory = tempfile::TempDir::new()?;
    let storage = storage_redb::Redb::open(directory.path().join("index.redb"))?;
    let repository = Repository::new(storage, scope())?;

    println!(
        "commit: {} blocks x {} tx x {} movements + {} created / {} spent  (~{} rows/block)",
        profile.blocks,
        profile.txs,
        profile.movements,
        profile.created,
        profile.spent,
        profile.rows(),
    );

    let mut parent: Option<BlockRef> = None;
    let mut spend: Vec<OutputKey> = Vec::new();
    let mut worst = 0.0_f64;
    let started = Instant::now();
    for height in 1..=profile.blocks {
        let (interpreted, next_spend) = interpret(&profile, height, parent.as_ref(), spend);
        let block = interpreted.block.clone();
        let addition = BlockAddition::new(scope(), parent.clone(), RETENTION, interpreted)?;
        let one = Instant::now();
        block_on(repository.add(addition))?;
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

    let pages = 50;
    let started = Instant::now();
    let mut transactions = 0;
    for index in 0..pages {
        let page = block_on(Transactions::list(
            &repository,
            HistoryQuery {
                scope: scope(),
                address: address(index % profile.addresses),
                after: None,
                limit: 100,
            },
        ))?;
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
        let page = block_on(Outputs::list(
            &repository,
            OutputRequest {
                scope: scope(),
                address: address(index % profile.addresses),
                after: None,
                limit: 100,
            },
        ))?;
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
