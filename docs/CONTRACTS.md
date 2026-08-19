# Public contracts

This document describes the reusable Rust boundaries. Current source is
authoritative for exact lifetimes, generic bounds, and error types.

## Wallet collection

`wallets::Wallets<I, F>` is the chain-neutral application surface. `I` is the
embedding application's wallet identity. `F` is its configured family key,
such as the public API's `Chain` enum.

```rust,ignore
let mut wallets = wallets::Wallets::new(checkpoints.clone());

wallets.register(
    Chain::Bitcoin,
    bitcoin_scope,
    bitcoin_provider,
    bitcoin_sender,
)?;
wallets.register(
    Chain::Ethereum,
    ethereum_scope,
    ethereum_provider,
    ethereum_sender,
)?;

let imported = wallets
    .import(id, &Chain::Bitcoin, secret, BlockHeight(birthday))
    .await?;
let generated = wallets.generate(other_id, &Chain::Ethereum).await?;
```

Each family registration contains exactly one `IndexScope`, concrete
`Provider`, and chain batch `Sender`. Duplicate family keys fail during
composition. A separate provider registry would duplicate that same family map
and is not part of the target API.

`Provider::create` imports secret bytes. Its default `Provider::generate` uses
the operating-system RNG before delegating to `create`. Both return an abstract
wallet and never return secret material.

Import requires a birthday. Generation assigns the next block after the
current checkpoint, or block zero when the scope has no checkpoint. `Wallets`
stores the abstract instance, public identity/scope/address metadata, and the
deduplicated `AddressFilter` used for synchronization.

`import` requires exclusive mutable access and is a startup-only operation.
After imports finish, composition wraps the collection in `Arc`; runtime code
can generate forward-only wallets but cannot add historical coverage.
Wallet lookup state uses a standard `RwLock`; operations clone the required
entry and release the lock before every `.await`.

The collection owns application operations:

```rust,ignore
let wallet = wallets.get(&id)?;
let balance = wallets.balance(&id).await?;
let page = wallets.history(&id, HistoryRequest::first(100)).await?;
let id = wallets.send(&id, destination, amount).await?;
let ids = wallets.send_all(transfers).await?;
let filters = wallets.filters()?;
```

Business and HTTP code use these methods without matching on the concrete
chain. The current registry is in memory. An embedding product that needs
durable custody loads encrypted secrets, identities, family keys, and
birthdays from its own trusted store before synchronization.

## Wallet capabilities

`Wallet` composes only capabilities that every constructed wallet actually
supports:

- `Addresser` and `AddressFormat` for canonical and external address forms;
- `BalanceReader` for exact balance at an indexed checkpoint;
- `HistoryReader` for complete checkpoint-bound history;
- `TransactionFactory` for a chain-backed transaction builder and broadcaster;
  and
- `Signer` for the minimal signing request.

Code needing one capability accepts that capability rather than `dyn Wallet`.
`Provider` is construction and does not become a post-construction wallet
capability.

One-wallet sending belongs to the wallet abstraction. It validates the
destination and exact positive amount, constructs the chain-native-backed
builder, prepares/signs, broadcasts the exact signed bytes, and verifies the
returned transaction ID. `Wallets::send` owns lookup and delegates this
operation.

A non-empty ordered batch belongs to the registered family's `Sender`:

- Bitcoin may fund one transaction from several abstract wallets, creates the
  requested outputs and per-source change, signs every input with its owner,
  and returns one submitted ID; and
- Ethereum prepares consecutive per-source nonces, broadcasts in input order,
  stops on the first failure, and returns the accepted prefix with the failed
  index.

All preflight validation occurs before the first external effect. Mixed-family
batches fail rather than being split. RPC acceptance means submitted; indexed
history establishes canonical inclusion and confirmation.

## Indexer

A one-chain service and multi-chain composer share one object-safe surface:

```rust,ignore
pub trait Checkpoint {
    fn checkpoint(&self, scope: &IndexScope)
        -> BoxFuture<Result<Option<BlockRef>, IndexError>>;
}

pub trait History {
    fn history(&self, query: HistoryQuery)
        -> BoxFuture<Result<TransactionPage, IndexError>>;
}

pub trait Indexer: Checkpoint + History {
    fn scopes(&self) -> &[IndexScope];

    fn sync(&self, filters: Vec<AddressFilter>)
        -> BoxFuture<Result<Vec<SyncStatus>, IndexError>>;
}
```

`Service<S, I, R>` implements `Indexer` for one exact scope. `Composer` requires
at least one child, rejects duplicate scopes, validates a complete filter
snapshot, partitions it by scope, routes checkpoint/history calls, and
synchronizes every child.

Synchronization policy stays concrete and small:

```rust,ignore
SyncConfig::new(scope, minimum_confirmations, reorg_retention, batch_size)?;
```

The three numeric inputs must be greater than zero. Confirmation depth is the
`u64` value itself; it does not need a one-field policy wrapper.

`Wallets` owns the filter snapshot and receives the composed `Checkpoint`
capability to choose safe runtime birthdays. Indexing owns no address registry
or watch lifecycle. The sync task repeatedly passes `wallets.filters()` to the
composed indexer.

Filter addresses are non-empty, unique, and scoped to a configured child.
Composer validates the whole snapshot before any source I/O; `Wallets` keeps
the earliest birthday when several wallets have the same canonical address.

`Outputs` is an independent capability. It is injected only into consumers
that need live UTXOs; it is not a supertrait of `Indexer`.

## Chain indexing contracts

Each chain implements:

- `BlockSource`, wrapping native RPC tip, block-at-height, and canonical hash
  reads; and
- `BlockInterpreter`, converting one native block and the active canonical
  addresses into `InterpretedBlock`.

`InterpretedBlock` contains a `BlockRef`, complete transaction drafts, and
`OutputChanges`. It contains no storage key, record, journal, or backend type.
Bitcoin and Ethereum remain free to use different native RPC and transaction
models.

## Persistence collections

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

`Blocks::add` compares the expected checkpoint and atomically writes canonical
address history, live output changes, a storage-derived bounded journal entry,
and the new checkpoint. `Blocks::remove` verifies the current tip and derives
the entire inverse from that private journal. No caller can author commit or
rollback state.

`Transactions` and `Outputs` are read projections over that atomic block
lifecycle. A persistence implementation may use RocksDB, PostgreSQL, or
another transactional backend, but backend records never cross these
contracts. Only RocksDB is implemented now.

## History model

`CanonicalTransaction` stores:

- scoped transaction identity;
- included or failed state tied to a canonical `BlockRef`;
- every stable `ValueMovement`; and
- optional exact network fee.

The `History` implementation reads canonical transactions and derives
`ObservedTransaction` confirmation from inclusion height and the page
checkpoint. `Confirmed` carries the observed depth. The current API neither
stores confirmation transitions nor claims chain-finality proof.

Bitcoin inputs and outputs are separate movements, so multi-input and
multi-output history remains truthful. Transfer, input, output, mint, and burn
are the current movement variants. Native and token assets have different
`AssetId` values. Wallet history resolves trusted asset metadata and converts
exact atomic values into exact display decimals.

History and output cursors carry the checkpoint snapshot from their first
page. A changed checkpoint produces a conflict and requires pagination to
restart.

## Address coverage contract

All imported wallets and birthdays for an existing scope form one
authoritative startup set. They are registered before the first sync. A fresh
scope anchors immediately before the earliest birthday and scans forward; an
empty scope anchors at the current tip.

A wallet created at runtime starts at `checkpoint + 1`. A historical address
cannot be added after `Wallets` becomes shared because import is startup-only.
If the authoritative startup set changes below the persisted checkpoint, the
embedding application recreates and rescans that scope. Indexing stores no
filter registry and cannot infer selection drift across restarts.

## Composition contract

The process is assembled directly in `apps/api/src/main.rs`:

1. read and validate configuration;
2. construct one long-lived RPC client per configured chain;
3. construct chain sources, interpreters, RocksDB repositories, and services;
4. combine the services in one concrete `Arc<Composer>`;
5. clone that object into narrow `Indexer`, `Checkpoint`, and `History` views;
6. expose the Bitcoin repository separately as `Outputs`, construct `Wallets`,
   register chain families, and import configured wallets;
7. start one sync loop with the current wallet filter snapshots;
8. wait for persisted ready checkpoints before binding HTTP; and
9. supervise HTTP, sync, fatal exits, cancellation, and graceful shutdown.

There is no process facade or app-local service facade. HTTP state contains the
abstract wallet collection and readiness state only. Concrete handles and
chain selection remain in `main`.

## HTTP contract

`apps/api` owns all public wire models, Utoipa schema derivation, extraction,
authentication, request limits, error/status mapping, and encoding. SDK crates
know no Axum route or response shape.

Each endpoint-specific input and output struct is declared immediately above
its handler. A handler performs extraction, one `Wallets` call, mapping, and
encoding. Catch-all DTO modules are not part of the target structure. A wire
type is shared only when several endpoints use the exact same contract;
reusable domain values remain in their SDK owner.

Public routes remain chain-neutral for wallet generation, metadata, balance,
paginated transactions, one send, and ordered batch send. Health distinguishes
liveness from index readiness. There is no public indexing command surface.
