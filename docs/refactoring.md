# Refactoring target

This document records the architecture currently being implemented. It is a
working design contract, not a compatibility promise and not a list of future
features. Exact Rust signatures remain authoritative in source.

## Outcome

Payment-SDK runs one process. `apps/api/src/main.rs` explicitly constructs the
Bitcoin and Ethereum stacks, combines their indexers, registers their wallet
families, imports configured wallets, starts synchronization, and exposes one
thin HTTP API.

After startup, application and endpoint code use two abstractions:

- `wallets::Wallets` creates, finds, reads, and sends from wallets; and
- `indexing::Indexer` synchronizes and queries one or several chain scopes.

Concrete chain types occur only while `main` wires the process. HTTP handlers
do not select a chain implementation, build a transaction, call RocksDB, or
manage synchronization. There is no process facade, app-local service facade,
indexer service, wallet service, or second internal transport.

The refactor is successful when deleting either concrete chain leaves the
generic packages, chain base, wallets, indexing, application abstractions, and
the other chain coherent.

## Ownership and dependency direction

```text
packages/*
    generic crypto, HTTP, JSON-RPC, and storage mechanics

sdk/chains/base
    approved chain-neutral values and signing/broadcast capabilities

sdk/indexing
    blocks, transactions, outputs, synchronization, reorgs, and queries

sdk/indexing/rocksdb
    physical indexing records, keys, atomic writes, and scans

sdk/wallets
    abstract wallets, family registration, address birthdays, and sending

sdk/chains/bitcoin       sdk/chains/ethereum
    native RPC, parsing, transaction building, wallet, and interpretation

apps/api
    concrete construction, task lifecycle, public HTTP, and OpenAPI
```

Dependencies point toward generic contracts. `packages/*` imports no SDK or
application crate. `sdk/indexing` imports no concrete chain, RocksDB record,
wallet type, or HTTP type. A chain interpreter emits domain facts and never a
database key. `apps/api` may import every crate because it is the composition
root; no crate imports it.

## Indexing API

### One reusable caller surface

One chain index and the multi-chain composer implement the same contract:

```rust,ignore
pub trait Indexer: Checkpoint + History {
    fn scopes(&self) -> &[IndexScope];

    fn sync<'a>(
        &'a self,
        filters: Vec<AddressFilter>,
    ) -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>>;
}
```

`Service<Source, Interpreter, Repository>` implements this contract for one
scope. `Composer` owns several `Arc<dyn Indexer>` values, rejects duplicate
scopes, partitions the supplied address snapshot by scope, and delegates every
checkpoint, history, and synchronization operation to the matching child.
Calling code therefore does not change when a process moves from one chain to
several.

`Composer` rejects an empty child set. Before any source I/O, synchronization
rejects empty, duplicate, or unconfigured-scope address values. `Wallets`
deduplicates repeated addresses by keeping their earliest birthday.

`AddressFilter` contains a canonical scoped address and its first relevant
block height. It is input state, not persistent indexing state:

```rust,ignore
pub struct AddressFilter {
    pub address: CanonicalAddress,
    pub start_height: BlockHeight,
}
```

`Wallets` owns and deduplicates this set. The synchronization task asks
`Wallets::filters()` for the complete current snapshot and passes it to
`Indexer::sync`. The indexer stores no watch, watch ID, filter registry, or
address lifecycle. Supplying the snapshot makes selection visible, testable,
and independent of the storage backend.

`Outputs` is a separate optional query capability. Bitcoin wallets need live
outputs; an account-chain indexer should not be forced to implement UTXO
semantics merely to satisfy `Indexer`.

### Chain boundary

Each chain supplies only two producer capabilities to generic indexing:

```rust,ignore
pub trait BlockSource {
    type Block: IndexedBlock;

    fn tip(&self) -> BoxFuture<Result<BlockRef, SourceError>>;
    fn block_at(&self, height: BlockHeight) -> BoxFuture<Result<Self::Block, SourceError>>;
    fn canonical_hash(&self, height: BlockHeight)
        -> BoxFuture<Result<Option<BlockHash>, SourceError>>;
}

pub trait BlockInterpreter {
    type Block: IndexedBlock;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError>;
}
```

The source wraps native RPC reads. The interpreter converts a native block
into an `InterpretedBlock` containing its canonical block reference, complete
transaction drafts, and live-output changes. Bitcoin keeps each input and
output as a separate movement. Ethereum keeps native transfers and token-log
transfers as distinct assets. Neither implementation knows how facts are
encoded in RocksDB.

### Persistence collections

Persistence is expressed by domain collections, not load/plan/apply phases:

```rust,ignore
pub trait Blocks {
    fn get(&self, selector: BlockSelector)
        -> BoxFuture<Result<Option<BlockRef>, IndexError>>;
    fn add(&self, addition: BlockAddition)
        -> BoxFuture<Result<BlockOutcome, IndexError>>;
    fn remove(&self, scope: IndexScope, expected_tip: BlockRef)
        -> BoxFuture<Result<Option<BlockRef>, IndexError>>;
}

pub trait Transactions {
    fn list(&self, query: HistoryQuery)
        -> BoxFuture<Result<CanonicalPage, IndexError>>;
}

pub trait Outputs {
    fn list(&self, request: OutputRequest)
        -> BoxFuture<Result<OutputPage, IndexError>>;
}
```

`BlockSelector::Tip(scope)` reads the canonical checkpoint.
`BlockSelector::Height { scope, height }` reads a retained canonical block used
to locate a reorg ancestor. `BlockAddition::new` validates scope, parent
connection, transaction identity, movement facts, output changes, and journal
retention before storage receives it.

`Blocks::add` is the only block write operation. Under one atomic storage
batch it:

1. compares the expected checkpoint with the current checkpoint;
2. verifies each required live output and derives rollback data from storage's
   own current state;
3. writes complete canonical transactions directly beneath every affected
   selected address;
4. creates and spends the live output projection;
5. writes one bounded rollback-journal entry;
6. moves the canonical checkpoint; and
7. prunes journal entries older than configured retention.

`Blocks::remove` accepts only the scope and expected current tip. It reads its
own retained journal, removes orphan history, restores created/spent outputs,
deletes that journal entry, and moves the checkpoint to the stored parent in
one atomic batch. A caller cannot supply undo data.

`Transactions::list` and `Outputs::list` are read projections. Their cursors
are bound to the checkpoint used for the first page. If canonical state changes
between pages, the caller restarts pagination. Confirmation is derived from
the inclusion height and the page checkpoint; `Confirmed` carries that observed
depth only. It is never stored as a mutable transition or presented as
chain-finality proof.

The persistence adapter stores only:

- the current canonical checkpoint for a scope;
- address-primary canonical transaction history;
- current live indexed outputs; and
- a bounded journal sufficient to reverse retained canonical blocks.

It does not store filters, synchronizer status, confirmation records,
observation revisions, pending transactions, an event feed, raw blocks, spent
markers, or a secondary address index.

### Initial synchronization and birthdays

All configured imported wallets are registered before the first sync. On a
fresh scope:

- with no selected addresses, synchronization establishes the source tip as an
  empty canonical anchor; a wallet generated afterward starts at the next
  height;
- with earliest birthday `B > 0`, synchronization establishes `B - 1` as the
  anchor and interprets `B..=tip`; and
- with birthday `0`, synchronization interprets from block zero.

At each height, only addresses whose birthday is at or before that height are
passed to the interpreter. The synchronizer resumes at `checkpoint + 1` after
a restart; it never scans from genesis merely because the database was empty.

A single scope checkpoint proves coverage only for the address snapshot used
to build it. Adding an imported address, or lowering an existing birthday, to
at or below an established checkpoint would silently omit history. Runtime
import is unavailable after `Wallets` is shared, and runtime generation is
forward-only. If the authoritative startup set changes beneath the checkpoint,
composition must recreate and rescan the scope with the complete set. There is
no per-address backfill or public rebuild command in the current design.

Before adding a new block, the synchronizer verifies the stored checkpoint is
still canonical. If it is not, it searches retained blocks for the common
ancestor and calls `Blocks::remove` until the checkpoint reaches it. If no
ancestor exists inside retention, it returns `ReorgTooDeep`; it does not invent
partial recovery.

## Wallet API

`Wallets<I, F>` is the application-facing collection. `I` is the
application-owned wallet identity and `F` is a small configured family key such
as the API's `Chain` enum. It owns:

- the family-to-`(IndexScope, Provider, Sender)` registrations;
- constructed abstract wallet instances and their public metadata;
- the canonical address/birthday set supplied to indexing; and
- one shared `Arc<dyn Checkpoint>` used to choose safe runtime birthdays.

The separate provider registry disappears because it duplicates family state.
`Provider` remains the two-operation concrete construction boundary.

The intended call surface is:

```rust,ignore
let mut wallets = Wallets::new(checkpoints.clone());
wallets.register(family, scope, provider, sender)?;

let imported = wallets
    .import(id, &family, secret, BlockHeight(birthday))
    .await?;
let generated = wallets.generate(other_id, &family).await?;

let summary = wallets.get(&id)?;
let balance = wallets.balance(&id).await?;
let history = wallets.history(&id, HistoryRequest::first(100)).await?;
let submitted = wallets.send(&id, destination, amount).await?;
let filters = wallets.filters()?;
```

`WalletInfo<I, F>` contains the application ID, family, exact `IndexScope`, and
external `AddressText`. `WalletTransfer<I>` contains a wallet ID, destination,
and exact amount. `send_all` returns ordered submitted IDs or `SendError` with
the accepted prefix, failed input index, and typed source error.

Import requires an explicit birthday. Generation chooses the block after the
current checkpoint, or block zero when no checkpoint exists. Neither operation
returns secret material. Wallet registry and key durability are supplied by
the embedding application; Payment-SDK's current in-process registry is not a
custody database.

`import` requires `&mut self` and therefore ends before composition wraps
`Wallets` in `Arc`. Runtime generation needs only shared access and is always
forward-only.

`Wallet::send` owns the shared one-wallet orchestration: validate the exact
positive amount and destination, create the chain-backed builder, prepare the
signed transaction, broadcast the exact signed bytes, and verify the submitted
ID equals the ID derived from those bytes. The concrete wallet still owns all
Bitcoin or Ethereum transaction semantics.

`Wallets::send_all` validates a non-empty, ordered, single-family batch before
delegating to that family's `Sender`. Bitcoin may combine multiple source
wallets into one native transaction. Ethereum emits ordered transactions and
reports the accepted prefix if a later broadcast fails. Submission is not
confirmation; clients read indexed history to observe canonical inclusion.

## Composition root

`apps/api/src/main.rs` deliberately shows the real object graph. It must not
hide unresolved construction behind a process or service facade. The shape is:

```rust,ignore
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("configuration path is required")?;
    let config = Config::read(path).await?;
    let token = http_support::server::BearerToken::new(
        std::env::var(&config.bearer_token_env)?,
    )?;
    let transport = if config.tls_terminated_upstream {
        http_support::server::TransportSecurity::TlsTerminatedUpstream
    } else {
        http_support::server::TransportSecurity::PlaintextLoopback
    };
    let server = http_support::server::Config::new(
        config.bind,
        transport,
        Some(token),
        http_support::server::RequestLimits::default(),
    );
    server.validate()?;

    // Build one Bitcoin RPC client, source, interpreter, repository, and
    // indexer when Bitcoin is configured. Retain the RPC/repository views the
    // wallet provider and sender need.
    let bitcoin_indexer = Arc::new(bitcoin::Indexer::new(
        bitcoin_source,
        bitcoin_interpreter,
        bitcoin_repository.clone(),
        bitcoin_sync,
    ));

    // Build the equivalent Ethereum indexing objects from startup config.
    let ethereum_indexer = Arc::new(ethereum::Indexer::new(
        ethereum_source,
        ethereum_interpreter,
        ethereum_repository,
        ethereum_sync,
    ));

    let composer = Arc::new(indexing::Composer::new(vec![
        bitcoin_indexer as Arc<dyn indexing::Indexer>,
        ethereum_indexer as Arc<dyn indexing::Indexer>,
    ])?);
    let indexer: Arc<dyn indexing::Indexer> = composer.clone();
    let checkpoints: Arc<dyn indexing::Checkpoint> = composer.clone();
    let history: Arc<dyn indexing::History> = composer.clone();
    let bitcoin_outputs: Arc<dyn indexing::Outputs> = Arc::new(bitcoin_repository);

    // Construct concrete providers/senders with narrow history, RPC, and
    // output views now that the composed query surface exists.
    let bitcoin_utxos = Arc::new(bitcoin::IndexUtxos::new(
        bitcoin_scope.clone(),
        bitcoin_network,
        bitcoin_outputs,
    )?);
    let bitcoin_provider = bitcoin::WalletProvider::new(
        bitcoin_wallet_config,
        bitcoin_utxos,
        bitcoin_fees,
        bitcoin_transactions,
        history.clone(),
    );
    let bitcoin_sender = bitcoin_provider.transactions();
    let ethereum_provider = ethereum::WalletProvider::new(
        ethereum_wallet_config,
        ethereum_accounts,
        ethereum_transactions,
        history,
    );
    let ethereum_sender = ethereum_provider.transactions();

    let mut wallets = wallets::Wallets::new(checkpoints);
    wallets.register(Chain::Bitcoin, bitcoin_scope, bitcoin_provider, bitcoin_sender)?;
    wallets.register(Chain::Ethereum, ethereum_scope, ethereum_provider, ethereum_sender)?;

    // Load/import configured wallets and their birthdays before the first sync.
    for configured in config.wallets {
        let bytes = hex::decode(std::env::var(&configured.secret_env)?)?;
        if bytes.len() != 32 {
            return Err("wallet secret must contain exactly 32 bytes".into());
        }
        wallets
            .import(
                configured.id,
                &configured.chain,
                wallets::SecretBytes::new(bytes),
                BlockHeight(configured.start_height),
            )
            .await?;
    }

    let wallets = Arc::new(wallets);
    let interval = Duration::from_millis(config.indexes.poll_millis());
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let (ready, mut ready_rx) = tokio::sync::watch::channel(false);
    let sync_wallets = wallets.clone();
    let mut sync = tokio::spawn(sync_task::run(
        indexer,
        move || sync_wallets.filters(),
        interval,
        shutdown_rx,
        ready,
    ));
    while !*ready_rx.borrow() {
        tokio::select! {
            changed = ready_rx.changed() => changed?,
            result = &mut sync => {
                return Err(format!("synchronization stopped during startup: {result:?}").into());
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                let _ = shutdown.send(true);
                sync.await??;
                return Ok(());
            }
        }
    }

    let state = api::State::new(wallets, ready_rx);
    let router = api::router(state, &server)?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let mut http = axum::serve(listener, router).into_future();
    let result = tokio::select! {
        result = &mut http => result.map_err(Into::into),
        result = &mut sync => result?.map_err(Into::into),
        result = tokio::signal::ctrl_c() => result.map_err(Into::into),
    };
    let _ = shutdown.send(true);
    if !sync.is_finished() {
        sync.await??;
    }
    result
}
```

The concrete `Arc<Composer>` is cloned and coerced independently to
`Arc<dyn Indexer>`, `Arc<dyn Checkpoint>`, and `Arc<dyn History>`. Bitcoin's
repository separately provides `Arc<dyn Outputs>` to its wallet provider.
`Composer` does not implement `Outputs`, because doing so would impose UTXO
semantics on account chains. These are narrow views, not parallel registries or
trait-object upcasts.

Startup order is part of correctness:

1. parse and validate configuration;
2. construct long-lived chain RPC clients and persistence adapters;
3. construct chain indexers and the composer;
4. register wallet families and imported wallets with birthdays;
5. start one synchronization loop using `Wallets::filters()`;
6. wait for every configured scope to have a persisted ready checkpoint;
7. bind the HTTP listener; and
8. supervise HTTP, synchronization, shutdown, and fatal task exits together.

## Business use without concrete chains

An embedding program may load an encrypted private key, wallet identity,
family key, and birthday from its own database during composition. After that
boundary, business code needs no Bitcoin or Ethereum imports:

```rust,ignore
async fn pay(
    wallets: &Wallets<WalletId, Chain>,
    wallet_id: &WalletId,
    destination: AddressText,
    amount: Decimal,
) -> Result<TransactionId, wallets::Error> {
    wallets.send(wallet_id, destination, amount).await
}

async fn activity(
    wallets: &Wallets<WalletId, Chain>,
    wallet_id: &WalletId,
) -> Result<History, wallets::Error> {
    wallets.history(wallet_id, HistoryRequest::first(100)).await
}
```

The same business functions work for any registered family. Chain selection,
RPC clients, signing rules, and transaction construction remain inside the
objects registered in `main`.

## HTTP boundary

HTTP state is data only:

```rust,ignore
#[derive(Clone)]
pub struct State {
    wallets: Arc<Wallets<String, Chain>>,
    readiness: watch::Receiver<bool>,
}

impl State {
    pub fn new(
        wallets: Arc<Wallets<String, Chain>>,
        readiness: watch::Receiver<bool>,
    ) -> Self {
        Self { wallets, readiness }
    }
}
```

Liveness always answers for the running process. Readiness reads the current
watch value and returns success only while all configured index scopes are
ready; it contains no second status model.

Every endpoint defines its endpoint-specific wire input and output immediately
above the one handler function:

```rust,ignore
#[derive(Deserialize, ToSchema)]
pub struct CreateWalletInput {
    pub chain: Chain,
}

#[derive(Serialize, ToSchema)]
pub struct CreateWalletOutput {
    pub id: String,
    pub chain: Chain,
    pub network: String,
    pub address: String,
}

pub async fn create(
    axum::extract::State(state): axum::extract::State<State>,
    Json(input): Json<CreateWalletInput>,
) -> Result<(StatusCode, Json<CreateWalletOutput>), ApiError> {
    let id = uuid::Uuid::now_v7().to_string();
    let wallet = state.wallets.generate(id, &input.chain).await?;
    Ok((StatusCode::CREATED, Json(wallet.into())))
}
```

The handler performs extraction, one domain call, error/status translation,
and encoding. A resource module may contain several endpoints, but each DTO
stays directly above the endpoint that owns it. A type is extracted only when
the exact same wire contract is used by several endpoints. Domain models do
not move into API DTO modules.

## Current non-goals

This refactor does not add deposit accounting, a ledger, collections/sweeps,
payment jobs, reservations, a public indexing command API, raw-block storage,
an event feed, per-address backfill, hardware wallets, remote custody, or a
PostgreSQL implementation. The collection contracts make another persistence
adapter possible; they are not evidence that one exists.

## Acceptance evidence

The architecture needs deterministic proof for:

- the same `Indexer` contract through one chain service and `Composer`;
- partitioning one authoritative address snapshot across chain scopes;
- birthday anchoring without a genesis scan, startup-only imports, and the
  required scope recreation when the historical startup set changes;
- restart from a height-and-hash checkpoint;
- atomic add and retained remove in RocksDB;
- duplicate add, checkpoint conflict, one- and multi-block reorg, and
  `ReorgTooDeep`;
- orphan history deletion and Bitcoin live-output restoration;
- checkpoint-bound history/output pagination;
- complete Bitcoin input/output movements and Ethereum native/token
  movements;
- `Wallets` generation/import, lookup, balance, history, one send, and batch
  send through abstract wallets;
- endpoint-local JSON/OpenAPI DTOs and thin handlers; and
- one-process startup, readiness, restart, fatal-task handling, and graceful
  shutdown using loopback RPC doubles and temporary RocksDB.

No public-network RPC or funded key belongs in these tests.
