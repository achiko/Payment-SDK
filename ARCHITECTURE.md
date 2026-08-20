# Architecture

## One process and one composition root

`apps/api` is the only executable and composition root. Its `main.rs` reads and
validates configuration, constructs concrete Bitcoin and Ethereum objects,
combines their indexing capabilities, registers wallet families, imports
configured wallets, starts synchronization, builds the router, and supervises
shutdown.

Construction is intentionally visible. There is no process facade, app-local
service facade, separate wallet process, separate indexer process, or internal
wallet/indexer HTTP protocol.

```text
main
  |- Bitcoin RPC/source/interpreter/repository/Service --\
  |                                                       -> Composer
  |- Ethereum RPC/source/interpreter/repository/Service -/
  |
  |- Bitcoin provider/sender --\
  |                             -> Wallets(instances, birthdays, Checkpoint)
  `- Ethereum provider/sender -/

sync task: Wallets::filters() -> Composer
HTTP: State { Wallets, readiness }
```

Each configured chain has one long-lived JSON-RPC client shared by its indexing
source and wallet-side capabilities. Retry and ordered endpoint failover are
configured once; the architecture does not invent a universal chain RPC
interface.

The concrete `Arc<Composer>` is cloned into narrow `Indexer`, `Checkpoint`, and
`History` trait-object views. Bitcoin separately receives an `Arc<dyn Outputs>`
view of its own repository; `Composer` does not force UTXO semantics onto
account chains. These are views of already-composed state, not parallel
registries.

## Dependency direction

```text
apps/api
  -> sdk/chains/{bitcoin,ethereum}
  -> sdk/wallets
  -> sdk/indexing
  -> sdk/indexing/rocksdb

sdk/chains/{bitcoin,ethereum}
  -> sdk/chains/base
  -> sdk/wallets
  -> sdk/indexing
  -> packages/{crypto,json-rpc}

sdk/wallets -> sdk/chains/base, sdk/indexing, packages/crypto
sdk/indexing/rocksdb -> sdk/indexing, packages/storage/rocksdb
sdk/indexing -> sdk/chains/base
sdk/chains/base -> packages/crypto
```

- A package may depend on external crates or another package, never SDK/apps.
- `sdk/chains/base` imports no concrete chain, RPC, indexing, or wallets.
- `sdk/indexing` imports no chain-native block, RocksDB record, wallet, or HTTP
  type.
- Concrete chain crates implement generic wallet/indexing contracts while
  retaining their native protocol semantics.
- `sdk/indexing/rocksdb` implements persistence collections only and owns no
  synchronizer runtime.
- No crate depends on `apps/api`.

## Ownership

### Generic packages

`packages/*` must remain useful outside a blockchain project:

- `crypto` owns secret memory and generic cryptographic operations;
- `http` owns small server/client mechanics;
- `json-rpc` wraps `jsonrpsee` with bounded transport, retry, and endpoint
  failover;
- `storage` owns backend-neutral atomic key/value mechanics;
- `storage/rocksdb` owns the generic RocksDB engine; and
- `design-lint` enforces repository architecture and API rules.

Packages contain no wallet, chain, indexing, asset, or transaction policy.

### Chain base

`sdk/chains/base` contains the explicitly approved values and capabilities
that genuinely apply across substantially different chains: addresses,
network/chain/asset metadata, exact decimals, block identity, derivation and
key/signature values, minimal signing, transaction snapshots, and broadcast
results.

It is not a home for UTXOs, Ethereum envelopes, chain RPC DTOs, wallet
construction, indexing policy, or a universal transaction representation.

### Concrete chains

A concrete chain owns everything that disappears when that chain is deleted:

- canonical address parsing and network validation;
- native RPC methods and wire DTOs;
- native block and transaction parsing;
- UTXO/script/fee or account/nonce/gas rules;
- transaction building, signing, encoding, and broadcasting;
- wallet provider, balance implementation, and batch sender; and
- block source and interpreter.

Bitcoin and Ethereum share an enforced directory skeleton but may have
different protocol-specific files. Equivalent boundaries use equivalent
directories; native semantics are not flattened to make file names identical.

Deleting one chain must leave the other chain and every generic crate coherent.

### Indexing

`sdk/indexing` owns:

- exact chain/network scopes and height-plus-hash checkpoints;
- address filters with birthday heights;
- canonical transaction, movement, and live-output facts;
- block-source/interpreter contracts;
- `Blocks`, `Transactions`, and `Outputs` persistence collections;
- the `Registry` collection holding the durable address selection;
- confirmation derivation and checkpoint-bound pagination;
- one-scope synchronization; and
- the multi-scope `Composer`.

One-chain `Service` and `Composer` implement the same `Indexer` trait. The sync
caller supplies a complete filter snapshot on every invocation; synchronization
itself holds no selection state.

`Registry` persists that selection so it survives a restart. It is a separate
collection, not an input to synchronization: a caller reloads the registry and
supplies the snapshot as before. Indexing stores each entry's opaque caller
material verbatim and never interprets it, so custody remains the embedding
application's decision.

`Blocks::add` atomically commits canonical history, live output changes, a
storage-derived bounded journal entry, and checkpoint movement. `Blocks::remove`
uses only that private journal to remove an orphan tip and restore live outputs.
`Transactions` and `Outputs` are read projections over this lifecycle.

`sdk/indexing/rocksdb` owns all indexing key encoding, records, scans,
compare-and-swap conditions, atomic batches, and journal encoding. Those types
never appear in a chain interpreter or generic consumer.

The durable set is deliberately limited to checkpoint, address-primary
canonical history, live outputs, a bounded rollback journal, and the registered
address selection. Confirmation, readiness, status, watches, revisions, raw
blocks, and event feeds are not persisted.

### Wallets

`sdk/wallets::Wallets<I, F>` owns the application-facing collection:

- a family map from `F` to scope, provider, and sender;
- abstract wallet instances and public metadata keyed by `I`;
- authoritative canonical address birthdays; and
- the shared `Checkpoint` capability used to choose safe runtime birthdays.

`Provider` constructs a concrete wallet by generating or importing secret
material without returning that secret. A separate provider map is redundant;
family registration owns the provider exactly once.

`Wallets` exposes startup-only import plus chain-neutral runtime generation,
get, balance, history, one send, and batch send operations. It delegates native
behavior to registered wallets and senders. Business and endpoint code do not
match on Bitcoin or Ethereum.

The wallet/key registry is in memory. Durable custody is the embedding
application's responsibility and is not represented as an indexing concern.

### Public HTTP

`apps/api` owns public routing, Utoipa schemas, transport validation,
authentication, limits, HTTP errors, and encoding. HTTP state contains the
abstract wallet collection and readiness state, not repositories, sources,
interpreters, or concrete chains.

Handlers are grouped by resource. Every endpoint-specific request and response
struct is declared immediately above its one handler. A handler is limited to
extraction, one `Wallets` operation, error/status mapping, and encoding. Shared
wire types exist only for exact reuse; domain types remain with their domain.

## Indexing flow

```text
Wallets::filters()
    -> Composer::sync
    -> partition filters by IndexScope
    -> chain Service
    -> verify local checkpoint against canonical hash
    -> retained reorg removal when needed
    -> source block at next height
    -> interpreter(native block, active addresses)
    -> BlockAddition::new
    -> Blocks::add
```

All configured historical wallets are registered before the first sync. A
fresh scope anchors immediately before its earliest birthday and scans forward;
an empty filter set anchors at the source tip. A generated wallet begins at the
next height.

The persisted checkpoint is valid for the authoritative historical address set
that produced it. A changed set below the checkpoint requires recreating and
rescanning the scope, because synchronization resumes from the checkpoint and
never revisits blocks behind it.

`Registry` records the selection that produced a checkpoint, so a restart
restores the same set rather than inferring one. It does not make a birthday
below the checkpoint safe: registering such an address still requires a rescan,
and callers are expected to reject or rescan rather than register silently.

A retained reorg removes orphan blocks until the common ancestor, then indexes
the replacement branch normally. When the ancestor is outside retention,
`ReorgTooDeep` requires a scope rescan.

## Transaction flow

```text
abstract wallet request
    -> concrete chain-native builder
    -> unsigned native transaction
    -> chain computes signing request
    -> injected Signer
    -> chain verifies and inserts signature
    -> exact signed bytes
    -> native broadcaster
    -> submitted transaction ID
    -> indexing observes canonical inclusion/confirmation/reorg
```

Submission is not confirmation. No broadcaster polls receipts to establish a
second confirmation system.

Bitcoin preserves outpoints, every input/output, scripts, checked satoshi
fees, per-input signers, and deterministic change. Ethereum preserves chain ID,
nonces, EIP-1559 fees, typed envelopes, recovered signer, receipts, and logs.
The shared wallet surface does not replace those native models.

For batches, validate every request and the one-family constraint before the
first external effect. Bitcoin may produce one multi-source native transaction.
Ethereum broadcasts an ordered sequence and reports its accepted prefix if a
later transaction fails.

## Runtime lifecycle

Startup order is part of correctness:

1. validate configuration and server security;
2. open chain clients and RocksDB repositories;
3. construct chain services and `Composer`;
4. construct `Wallets`, register families, and import startup wallets;
5. start one sync loop with `Wallets::filters()`;
6. wait for every configured scope to report `Ready` with a persisted
   checkpoint;
7. bind the public listener; and
8. supervise HTTP, synchronization, Ctrl-C, cancellation, and task joins.

`SyncStatus` reports only progress (`CatchingUp` or `Ready`). Failures are typed
errors, not cached status variants. A fatal synchronizer exit fails startup or
terminates runtime rather than serving stale data.

## Product boundary

The architecture currently supports wallet generation/import, canonical
address and exact balance, complete paginated history, one or ordered batch
submission, and continuous filtered indexing.

It does not contain deposit accounting, ledgers, payment state machines,
collection/sweep jobs, reservations, hardware-wallet workflows, remote
custody, public index-management commands, raw-block archives, event feeds, or
pre-release compatibility layers.
