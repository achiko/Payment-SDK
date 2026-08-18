# Public contracts

This document explains how reusable Rust boundaries compose. Exact signatures
live in source and are authoritative.

## Wallet construction

```rust,ignore
let mut providers = wallets::Providers::new();
providers.register(bitcoin_key, bitcoin_provider)?;
providers.register(ethereum_key, ethereum_provider)?;

let wallet: Arc<dyn wallets::Wallet> = providers.generate(&bitcoin_key).await?;
let imported = providers.create(&ethereum_key, secret).await?;

let mut wallets = wallets::Wallets::new();
wallets.insert(wallet_id, wallet)?;
```

`Providers<K>` is a constructor map owned by the application. `K` is an
application-defined typed key describing a configured chain/network/asset.
`Provider::create` imports caller-supplied secret material, while its default
`Provider::generate` uses the OS RNG before delegating to `create`. Neither
operation exports a secret. `Wallets<K>` separately maps application-owned
wallet IDs to already-created `Arc<dyn Wallet>` values; request handlers never
select a concrete provider.

`Wallet` is the intersection of deliberately small capabilities. Important
ones are:

- `Addresser` and `AddressFormat` for canonical bytes and external text;
- `BalanceReader` for the current indexed exact balance;
- `HistoryReader` for complete paginated transaction history;
- `TransactionFactory` for a chain-native-backed builder and broadcaster;
- `Signer` for the builder's minimal cryptographic requests.

`Provider` is the separate two-method construction boundary; it is not a
wallet capability after construction.

Code needing only an address accepts `&dyn Addresser`; code needing history
accepts `&dyn HistoryReader`. `dyn Wallet` is appropriate only at the API
facade where the full composed capability is actually required.

Concrete chain and base crates retain approved chain-native transaction and
minimal signing capabilities. `wallets::Wallet` composes them behind
`TransactionFactory`, and the API uses that boundary for one ordinary transfer.
The one-method `Transactions::send(Vec<Transfer>)` capability owns batch
submission; each `Transfer` carries an already-resolved abstract wallet,
external destination, and exact amount.

## Sending

For one transfer, the API asks the abstract wallet for a builder, adds one
destination/amount, prepares the signed chain-native transaction, and submits
it through the wallet's broadcaster. The response transaction ID must equal the
ID computed from the signed bytes.

`TransactionFactory` has three operations: create an empty builder, restore a
builder snapshot, and expose its broadcaster. A snapshot is versioned JSON that
contains transaction intent only. Restoring through `dyn Wallet` verifies the
snapshot kind, chain, network, asset, and source-wallet identity, then injects
that wallet's live signer, RPC adapters, and fee policy. Private keys and RPC
handles are never serialized. Bitcoin snapshots retain every destination and
amount in the UTXO transfer set; Ethereum snapshots retain its single native or
ERC-20 transfer.

A batch is not a loop with identical semantics on every chain:

- Bitcoin groups requested outputs by source wallet, reads every source's UTXOs
  at one checkpoint, preserves per-source change, signs inputs with their
  owners, and broadcasts one transaction; and
- Ethereum prepares consecutive-nonce transactions and broadcasts them in
  order. A later failure cannot undo the already accepted prefix.

Confirmation is read from indexed history. No sender polls receipts or creates
a durable payment/collection state machine.

## Indexing consumers

Consumer traits are small:

```rust,ignore
pub trait Checkpoint { /* checkpoint(scope) */ }
pub trait Watcher { /* watch(request) */ }
pub trait History { /* transaction(query), history(query) */ }
```

`Index<R>` is the consumer facade over one repository. Its available traits are
determined by `R`'s bounds. One instance represents one exact `IndexScope`; the
application selects the configured instance at its composition boundary.

An API wallet generally uses `History` and the output/balance query capability
provided by its concrete provider. An API handler never opens RocksDB or calls
a chain interpreter directly.

## Indexing producers

Each chain provides:

- a `BlockSource` that reads canonical chain-native blocks from its RPC client;
- a `BlockInterpreter` that inspects a block against the active watch snapshot;
  and
- chain-owned effect/undo values sufficient to update and roll back its
  projection.

`Synchronizer` coordinates source, interpreter, and five semantic persistence
traits: `CanonicalStore`, `WatchStore`, `BlockStore`, `HistoryStore`, and
`StatusStore`. Each trait has two cohesive methods.

`apps/api` opens storage, constructs a
`sdk/indexing/rocksdb::Repository`, gives a clone to `Synchronizer`, and
wraps another clone in `Index<R>`. The RocksDB adapter has no runtime or public
synchronizer handle; task ownership belongs to the application.

This concrete construction is written directly in `apps/api/src/main.rs`.
`payment-api` deliberately exports no runtime, configuration, index handle, or
storage facade: its library surface is the typed HTTP contract and the
abstraction-backed `Gateway` state used by handlers.

The concrete construction details differ by chain but remain in `apps/api`
startup. Routes receive consumer contracts, not repositories or synchronizers.
The application owns a shutdown channel and task set directly; on shutdown it
signals and joins every synchronizer, while any fatal task exit terminates the
process.

The current watch target is a canonical address. There is no transaction
watch, unwatch, event feed, raw-block archive, backfill/rebuild command, or
migration API. Reorg correction remains represented by durable observation
revisions queryable through history.

## History model

An `ObservedTransaction` contains:

- scoped transaction identity and monotonically revised observation identity;
- pending/included/confirmed/failed/replaced/dropped/reorged status;
- zero or more stable `ValueMovement` values;
- optional exact network fee; and
- first-seen and latest-observed ordering fields.

UTXO transactions use separate input and output movements. Account chains use
transfers; tokens use their own asset ID. `wallets::History` resolves trusted
asset metadata and converts atomic integer facts into exact display decimals.

## HTTP boundary

`apps/api` maps public JSON DTOs to these in-process contracts. Its resource
modules group wallet, transaction, health, and OpenAPI-contract handlers;
Utoipa derives `/openapi.json` from those same routes. No SDK crate depends on
Axum, public route names, bearer tokens, or HTTP response shapes. `packages/http` supplies
generic authentication and request-limit mechanics only.
