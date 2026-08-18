# Architecture

## One application

`apps/api` is the only executable and the only composition root. Its startup
code constructs concrete Bitcoin and Ethereum RPC clients, RocksDB indexing
repositories, synchronization synchronizers, wallet providers, and the HTTP router.
Those objects communicate directly in-process.

Each configured chain establishes one JSON-RPC client shared by its block
source and wallet-side fee/account/transaction capabilities. Endpoint failover
therefore has one configuration and does not create an artificial RPC service
interface.

```text
HTTP request
    -> apps/api facade
        -> sdk/wallets abstraction
            -> sdk/chains/bitcoin or sdk/chains/ethereum
        -> sdk/indexing abstraction
            -> chain block source + interpreter
            -> sdk/indexing/rocksdb repository

background task
    -> chain block source
    -> sdk/indexing Synchronizer
    -> sdk/indexing/rocksdb repository
```

There is no `apps/wallet`, `apps/indexer`, remote indexing client, or internal
wallet/indexer HTTP protocol. HTTP is only the public edge of `apps/api`.
Public handlers are grouped by resource (`wallet`, `transaction`, `health`,
and `contract`) and generate one OpenAPI contract through Utoipa. Resource
modules translate HTTP DTOs only; `Gateway` owns in-process application
behavior.

## Ownership

### Packages

`packages/*` contains infrastructure that can be copied to a non-blockchain
project without bringing business concepts with it:

- `crypto`: secret memory and generic cryptographic operations;
- `http`: small client/server mechanics;
- `json-rpc`: bounded `jsonrpsee` transport, retry, and endpoint failover;
- `storage`: atomic key/value mechanics;
- `storage/rocksdb`: generic RocksDB engine; and
- `design-lint`: repository architecture and API checks.

A package may depend on another package or an external library. It must not
depend on `sdk/*` or `apps/*`.

### Chain base

`sdk/chains/base` contains only approved values and capabilities that genuinely
apply across chains: address bytes and formatting contracts, network/chain/
asset metadata, exact decimal amounts, block identity, signing inputs and
outputs, transaction builder snapshots, and broadcasting.

It does not contain indexing policy, RPC DTOs, UTXOs, Ethereum envelopes,
wallet construction, or a universal transaction representation.

### Concrete chains

Each concrete chain owns everything whose semantics disappear when that chain
is removed:

- canonical address parsing and network validation;
- RPC methods and wire DTOs;
- chain-native blocks and transaction types;
- fee, nonce, UTXO, script, gas, and signing rules;
- wallet provider and implementation; and
- block source and interpreter for generic indexing.

Every concrete chain has the same ownership skeleton:

```text
src/address.rs
src/batch.rs
src/error.rs
src/lib.rs
src/indexer/mod.rs
src/rpc/mod.rs
src/transaction/mod.rs
src/wallet/mod.rs
```

Protocol-specific files may be added below these boundaries. The
`chain-layout` design-lint rule rejects missing paths and complex modules
declared as a single file instead of a directory module.

Deleting Bitcoin must leave Ethereum, base, wallets, indexing, and packages
compilable. The inverse applies to Ethereum.

### Wallets

`sdk/wallets` defines the application-facing capabilities. Traits remain small
and composable: address formatting, balance, history, sending, generation, and
provider construction. `Wallets<K>` maps an
application-owned ID to an already-created abstract wallet. `Providers<K>`
maps a typed chain/configuration key to the concrete provider selected at
startup.

The wallet send boundary is one operation over a non-empty list of transfers.
It delegates protocol construction/signing to the concrete chain:

- Bitcoin accepts several source wallets, reads every source at one canonical
  checkpoint, and creates one transaction with their inputs, requested outputs,
  and distinct per-source change; and
- Ethereum creates one transaction per requested transfer, assigns consecutive
  nonces per source wallet, and broadcasts them strictly in input order.

This is batch submission, not a collection/accounting domain and not a
universal chain transaction object.

The wallet API returns exact amounts and complete transaction movements. It
does not flatten a multi-input/multi-output UTXO transaction into a fictional
single transfer.

### Indexing

`sdk/indexing` owns chain-neutral synchronization and query semantics:

- canonical checkpoints contain block height and hash;
- watches identify canonical addresses and have a birthday height;
- block interpretations contain normalized transaction movements plus a
  chain-owned opaque effect used for rollback;
- block effects, undo data, observation revisions, and checkpoint
  movement commit atomically;
- reorgs append corrected revisions rather than erase history; and
- history cursors are stable and scoped to one chain/network.

`Index<R>` exposes checkpoint, address-watch registration, and history without
exposing the repository implementation. `Synchronizer` consumes the same
repository through five semantic persistence traits: `CanonicalStore`,
`WatchStore`, `BlockStore`, `HistoryStore`, and `StatusStore`. Each trait has
exactly two cohesive methods. `sdk/indexing/rocksdb::Repository`
implements those contracts and owns all physical encoding. A future PostgreSQL
adapter can implement them without changing a chain interpreter or wallet.

## Transaction flow

Transactions remain chain-native throughout protocol work:

```text
chain-native request
    -> unsigned chain-native transaction
    -> chain computes payload/digest
    -> injected Signer signs it
    -> chain validates and inserts signature
    -> signed chain-native bytes
    -> chain broadcaster submits bytes
    -> indexer observes inclusion, confirmation, or reorg
```

The generic builder interface exists for application orchestration and emits a
JSON-serializable snapshot. The originating wallet restores that snapshot and
reinjects its live signer, RPC client, and chain policy after validating the
version, chain, network, source wallet, asset, and transaction kind. The
snapshot contains intent only; it does not serialize secrets or replace Bitcoin
inputs/outputs or Ethereum typed transactions with a universal model.

For multi-transfer sending, validation and preparation occur before the first
external effect. Bitcoin allocates the fee deterministically across funding
groups, signs every input with its owning wallet, and has one broadcast result.
Ethereum cannot make
several broadcasts atomic: it stops at the first failure and must report the
accepted prefix precisely. RPC acceptance remains submission rather than
confirmation.

## Runtime rules

- RPC submission means submitted, not confirmed. Confirmation is an indexing
  fact; the API does not poll receipts with a second confirmation subsystem.
- The public listener starts only after every configured embedded index reaches
  `SyncPhase::Ready` with a persisted canonical checkpoint; a fatal synchronizer exit
  fails startup or terminates runtime.
- Money uses checked integer atomic units and `Decimal` for exact display-unit
  conversion. Floating point is forbidden.
- Private keys never appear in logs, serialization, responses, or `Debug`.
- The current in-process key pair is development custody, not a production
  HSM/KMS boundary.
- Startup owns task supervision and graceful shutdown for both indexing synchronizers
  and HTTP serving.
