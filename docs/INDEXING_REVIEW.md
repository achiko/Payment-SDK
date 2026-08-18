# Indexing simplification review

## Decision summary

Indexing has two responsibilities:

1. follow a chain safely and record transaction facts;
2. let callers watch addresses or transactions, read history, and consume changes.

The current implementation separates those responsibilities through consumer traits and focused
repository traits. This document describes that implemented boundary and records remaining limits;
it does not specify an additional streaming API.

The intended layering is:

```text
business code -> Watcher + History + Observer
                         |
apps/indexer composes chain source + interpreter + repositories
                         |
sync engine -> CanonicalReader + WatchReader + ChainWriter + StatusStore
                         |
RocksDB adapter now / PostgreSQL adapter later
```

The sync engine may require one shared database transaction for a block. That does not justify one
public repository containing every query and maintenance operation.

## What exists today

The core `sdk/indexing` crate exports the normalized fact model, three consumer traits, sync
contracts, and focused persistence capabilities. `sdk/indexing/rocksdb` owns the durable adapter.
`apps/indexer` initializes the selected chain source, worker, repository, and HTTP routes.

### Consumer API

`Watcher`, `History`, and `Observer` are the one consumer surface. `Indexer` is only their blanket
marker; it does not expose synchronization. `indexing_rocksdb::Repository` implements all three.
`Composer` delegates them by exact scope using `Composer::new().with(scope, child)?`. Registering
the same scope twice returns a conflict without replacing the first child.

Synchronization stays behind the service facade rather than the consumer marker. `Composer` is
built incrementally and only delegates one scoped request at a time. There is no generic HTTP
client implementing these traits. The current HTTP server exposes the same semantic operations,
but remote trait adaptation and a full external end-to-end confirmation workflow remain
unimplemented.

### Repository coupling

The worker names the focused capabilities it uses rather than an aggregate marker. The ordinary
synchronization path uses:

- `checkpoint` and `canonical_block`;
- `watches_at`;
- `commit_block` and `revert_tip`;
- `status` and `set_status`.

History, event reads, backfill, and rebuild administration remain separate capabilities.

### Opaque projections

`ProjectionQuery` exposes byte prefixes, byte keys, and byte values. That is a persistence protocol,
not a business abstraction. The Bitcoin HTTP API knows how to construct UTXO key prefixes and
decode stored values. A PostgreSQL implementation would either have to emulate RocksDB keys or
retain a second adapter layer solely to undo this leak.

Bitcoin should expose a semantic `Utxos` repository in the Bitcoin crate. The RocksDB adapter can
implement it using its current projection records. PostgreSQL can implement it with UTXO columns
and indexes. Generic indexing may keep an internal projection mechanism for atomic block commits,
but callers must not scan opaque keys.

## Proposed public API

The business-facing API needs three small traits. Names are short because the module supplies the
context.

```rust
pub trait Watcher: Send + Sync {
    fn watch<'a>(&'a self, request: WatchRequest)
        -> BoxFuture<'a, Result<WatchReceipt, IndexError>>;
    fn unwatch<'a>(&'a self, request: UnwatchRequest)
        -> BoxFuture<'a, Result<UnwatchOutcome, IndexError>>;
}

pub trait History: Send + Sync {
    fn transaction<'a>(&'a self, request: TransactionQuery)
        -> BoxFuture<'a, Result<Option<ObservedTransaction>, IndexError>>;
    fn history<'a>(&'a self, request: HistoryQuery)
        -> BoxFuture<'a, Result<TransactionPage, IndexError>>;
}

pub trait Observer: Send + Sync {
    fn events<'a>(&'a self, request: EventQuery)
        -> BoxFuture<'a, Result<EventPage, IndexError>>;
}

pub trait Indexer: Watcher + History + Observer {}
```

`Indexer` is only a convenient trait object/marker. `indexing_rocksdb::Repository` and `Composer`
implement the focused traits. Following a chain is a service responsibility, not a method business
code calls on its history client.

An event cursor provides durable incremental observation:

```rust
let watch = indexer.watch(watch_request).await?;
let page = indexer.history(history_query).await?;
let changes = indexer.events(event_query).await?;
```

Wallets only need `Arc<dyn History>`. Deposit registration needs `Arc<dyn Watcher>`. The payment
event mirror needs `Arc<dyn Observer>`. None should receive the full sync engine. The caller owns
polling and persists `EventPage::next`; the API does not produce a follow stream.

## Repository API

Repositories are grouped by behavior without one god trait. The implemented names are
`CanonicalReader`, `WatchReader`, `WatchStore`, `ChainWriter`, `TransactionReader`, `EventReader`,
`StatusStore`, the backfill traits, and the rebuild traits. `WatchStore` cohesively contains both
registration and removal.

Historical backfill and rebuild administration remain separate internal/operational capabilities.
The normal worker bound names only the capabilities it consumes. The HTTP read API uses the
consumer traits implemented by the repository. No aggregate repository marker appears in ordinary
generic bounds.

RocksDB handles may share one internal engine and one transaction. PostgreSQL repositories may
share a pool/transaction coordinator. The trait split controls semantics; it does not require a
separate physical database connection per trait.

## Amount decision

`AtomicAmount` is removed in the implemented design and replaced by
`base::Decimal`.

`AtomicAmount` duplicates a subset of `Decimal`: it is an unsigned 256-bit integer with custom
base-10 parsing, formatting, addition, and subtraction. It forces every indexed value and every
payment ledger value to be expressed in chain atomic units, so generic callers still need external
asset decimals to display or request `1 BTC` or `1 ETH`.

`Decimal` already provides:

- arbitrary precision;
- exact base-10 fixed-point representation;
- conversion from and to chain atomic units using asset decimals;
- no floating point;
- canonical display.

Before the amount migration, movement shape must also stop encoding UTXO
transactions as nullable account transfers. The implemented shape is an enum:

```rust
pub enum Movement {
    Transfer { id: MovementId, asset: AssetId, amount: Amount, from: Address, to: Address },
    Input { id: MovementId, asset: AssetId, amount: Amount, owner: Option<Address> },
    Output { id: MovementId, asset: AssetId, amount: Amount, owner: Option<Address> },
    InternalTransfer { id: MovementId, asset: AssetId, amount: Amount, from: Address, to: Address },
    Mint { id: MovementId, asset: AssetId, amount: Amount, to: Address },
    Burn { id: MovementId, asset: AssetId, amount: Amount, from: Address },
}
```

Each Bitcoin input and output becomes an independent movement. There is no
fabricated pairing between inputs and outputs. An owner remains optional only
when a chain script cannot be represented as a canonical address. Account
transfers, minting, and burning require the endpoints their semantics demand.
`Amount` is `base::Decimal`. Existing indexing data is rebuilt because the
record shape is intentionally incompatible; no compatibility migration API is
added to indexing.

Chain interpreters convert once at their boundary:

```rust
let amount = BTC.from_atomic(BigUint::from(satoshis));
let amount = ETH.from_atomic(wei);
```

The amount boundary requires these safeguards:

1. add non-negative checked subtraction and addition helpers to `Decimal` for accounting;
2. reject negative movement and fee amounts at the indexing commit boundary;
3. define a stable decimal record encoding as sign/coefficient/scale, not Rust `BigInt` layout;
4. encode JSON amounts as canonical strings;
5. update deposits and collection accounting in the same change so there is only one money type;
6. retain chain-native integer types (`Satoshi`, `Wei`, token units) while building and signing.

This does not mean transaction construction uses decimal arithmetic internally. Builders convert a
user-facing `Decimal` to the chain's exact atomic integer once, validate precision/range, and remain
integer-only afterward.

## Type simplification

The consumer module should use module context instead of repeated adjectives:

| Current | Proposed | Reason |
|---|---|---|
| `IndexScope` | `Scope` | already inside `indexing` |
| `CanonicalAddress` | `Address` | include the complete `Scope`, including network |
| `TransactionRef` | `TransactionId` | include `Scope`; canonical is an invariant |
| `ObservedTransaction` | `Transaction` | every value returned by indexing is observed |
| `ValueMovement` | `Movement` | value is represented by `amount` |
| `NetworkFee` | `Fee` | module and asset provide context |
| `ObservationEvent` | `Event` | indexing events are observations by definition |
| `ObservationRevision` | `Revision` | same reason |
| `ObservationDraft` | `Draft` | engine-only module supplies context |

The scoped-identity correction is implemented while the shorter-name proposal remains optional:
`CanonicalAddress` and `TransactionRef` now carry one complete `IndexScope`. Consumer, HTTP, and
persistence boundaries reject a mismatched request scope, and RocksDB keys include chain, network,
and canonical text. This greenfield storage layout contains no compatibility decoder for earlier
network-less identities.

`TransactionStatus` advertises pending, replaced, and dropped states although the implemented
indexers are block-only. Those variants should be removed until a mempool implementation can prove
their semantics. Inclusion and confirmation can share one `Inclusion` value:

```rust
pub struct Inclusion {
    pub block: BlockRef,
    pub confirmations: u64,
    pub finalized: bool,
}

pub enum Status {
    Included(Inclusion),
    Confirmed(Inclusion),
    Failed { inclusion: Option<Inclusion>, reason: Option<String> },
    Reorged { previous: BlockRef },
}
```

Stable movement IDs, transaction revisions, and event IDs remain necessary. The payment service
uses them for classification, idempotency, correction, and ledger attribution; they are not
ceremonial identifiers.

## Namespace split

The `indexing` crate root should expose only the consumer model and traits. Implementation-facing
types remain public for cross-crate adapters but live under explicit modules:

```text
indexing::{Indexer, Composer, Watcher, History, Observer, ...}
indexing_rocksdb::{Repository, Records, ...}
indexer_worker::{EthereumService, EthereumConfig, BitcoinService, BitcoinConfig}
```

This makes the common use case readable without deleting the tested reorg machinery.

## Remaining work

- Implement a generic remote client for the consumer traits if applications need remote trait
  objects rather than the current HTTP DTO boundary.
- Add a true process-level end-to-end test covering prepare, durable exact-envelope persistence,
  broadcast, watch registration, indexing, and observer consumption.
- Wire confirmation workflows explicitly in an application; `Broadcaster` intentionally returns
  submission only and does not wait on `Observer`.
- Add a PostgreSQL adapter and run a shared repository contract suite before claiming backend
  parity.

## Acceptance criteria

- A wallet can be constructed with `Arc<dyn History>` and cannot start synchronization or mutate
  watches.
- A deposit workflow can use `Arc<dyn Watcher>` and `Arc<dyn Observer>` without importing repository
  or storage types.
- `sdk/indexing` has no dependency on RocksDB, generic storage, or a concrete chain.
- No business API contains opaque projection keys or values.
- The ordinary sync worker does not require history, events, migrations, or rebuild administration.
- Bitcoin and Ethereum produce the same concise `Transaction`/`Movement` model while retaining
  chain-native block, RPC, UTXO, receipt, and signing types in their own crates.
- All generic amounts use `Decimal`; chain builders retain exact native integer units.
- Reorg tests still prove append-only revisions, correction events, and atomic checkpoint movement.
- The RocksDB repository implements `Watcher`, `History`, and `Observer` directly.
