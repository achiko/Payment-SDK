# Architecture rules

## Layers

```text
apps
├──> sdk/deposits ──> sdk/indexing ──> sdk/storage
├──> sdk/chains/<concrete> ──> sdk/{chains/contract, transactions, signing, indexing}
└──> packages

sdk/chains/contract ──> sdk/{chains/identity, signing}
sdk/indexing ──> sdk/{chains/identity, storage}
sdk/* ──> packages/* where generic transport is required
packages/* ──> packages/* only
```

`A -> B` in a Cargo graph means “A depends on B.” Therefore the abstraction
order `storage -> indexing -> bitcoin` appears in Cargo as
`bitcoin -> indexing -> storage`.

`apps/`, `sdk/`, and `packages/` are architectural namespaces. Every leaf with
a `Cargo.toml` is a Cargo package; packages do not sit directly at the
repository root.

## Ownership

- `apps/` selects concrete chains, signers, storage, transports, and workers.
- `apps/api/` is the PS composition root and owns one Ethereum scope's
  user/deposit orchestration, PS RocksDB, policy, jobs, and business workers.
- `apps/custody/` is a loopback-only, ephemeral local-development adapter over
  `signer-local`; it is not durable or production custody.
- `apps/indexer/` is the IX composition root and owns its
  checkpoint/watch/observation DB. Its library facade and thin CLI host the
  same runtime; embedding does not weaken exclusive database ownership.
- `apps/wallet/` is the stateless WS composition root and must not select or own
  a storage backend.
- `sdk/chains/identity/` owns only opaque cross-process chain, asset, address,
  transaction, and 256-bit atomic-value identifiers.
- `sdk/chains/contract/` owns small stateless wallet/transaction capabilities.
- `sdk/chains/bitcoin/` owns every Bitcoin-specific type and rule.
- `sdk/chains/ethereum/` owns every Ethereum-specific type and rule.
- `sdk/transactions/utxo/` owns reusable selection, fee, output, and change
  algorithms, but no Bitcoin serialization or signing.
- `sdk/transactions/account/` owns only behavior genuinely shared by
  account-model transactions.
- `sdk/signing/` owns chain-independent keys and cryptographic operations.
- `sdk/indexing/` owns synchronization, checkpoints, watches, changes, and
  reorg/finality orchestration without knowing a concrete chain.
- `sdk/deposits/` owns PS-only deposits, observation classification, event-log,
  accounting-ledger, and durable collection-workflow contracts.
- `sdk/storage/` owns atomic persistence mechanics without knowing chains or
  indexer semantics.
- `packages/` contains code transferable to a non-blockchain project.

The chain deletion test is mandatory: deleting `sdk/chains/bitcoin/` must
remove every Bitcoin-specific type while leaving signing, UTXO construction,
indexing, storage, HTTP, and JSON-RPC usable.

## Explicitly rejected designs

- Do not return to a single flat `crates/` directory.
- Do not distribute one chain across global `ports`, `domain`, `primitives`, or
  adapter buckets.
- Do not create catch-all packages named `core`, `common`, or `utils`.
- Do not introduce `signing-core`, `signer-bitcoin`, or `signer-ethereum`.
- Do not place `local.rs` or `trezor.rs` in a chain or wallet directory.
- Do not let a chain choose or construct a concrete signer.
- Do not make generic signing depend on transaction, wallet, RPC, or indexer
  types.
- Do not introduce a `signing_plan` layer. Use builder, unsigned transaction,
  and signed transaction states.
- Do not put concrete Bitcoin or Ethereum RPC methods in generic JSON-RPC.
- Do not make a storage backend part of generic chain/signing contracts.
  Applications may select a backend after its semantics are approved; Ethereum
  IX v1 selects RocksDB through `storage-rocksdb`.
- Do not assume all account-oriented chains use Ethereum's nonce/value/gas
  transaction model.
- Do not name an application `payment-service`; name executables after their
  actual role, such as `api`, `worker`, or `cli`.
- Do not add a dependency from a more generic layer to a less generic layer.

## Current dependency graph

```text
apps/api
├── deposits
├── indexing + chain-identity
├── chain-ethereum (signed-envelope inspection only)
├── storage-rocksdb
└── packages/http + telemetry

apps/custody                        (local development only)
├── signer + signer-local + signer-remote wire DTOs
└── packages/http (loopback-only; no storage backend)

apps/indexer                         (Ethereum v1 composition)
├── chain-ethereum
├── indexing
├── storage-rocksdb
└── packages/http + telemetry

apps/wallet
├── chain-bitcoin / chain-ethereum
├── signer + signer-remote
└── packages/http + telemetry (no direct storage or DB backend)

chain-bitcoin
├── chain-contract
├── transaction-utxo
├── signer
├── indexing
└── json-rpc

chain-ethereum
├── chain-contract
├── transaction-account
├── signer
├── indexing
└── json-rpc + packages/http

deposits      -> indexing + chain-identity + signer
indexing      -> storage + chain-identity
chain-contract -> chain-identity + signer
signer-local  -> signer
signer-remote -> signer (external reqwest transport; no chain dependency)
signer-trezor -> signer + transport
json-rpc      -> transport
http          -> transport
packages/*    -> packages/* only
```

## Ethereum Indexer v1 selection

[`docs/INDEXER_SERVICE.md`](./docs/INDEXER_SERVICE.md) is the approved concrete
selection for the first IX vertical slice. It does not weaken the ownership
rules above:

- `sdk/indexing` owns backend-independent ordered sync and semantic repository
  commands; the application injects `storage-rocksdb`.
- `sdk/chains/ethereum` owns Ethereum RPC methods, decoding, and fact drafts.
- `apps/indexer` selects one Ethereum scope, source, repository, HTTP adapter,
  telemetry, and worker supervisor. `IndexerService` exposes that composition
  to an in-process application, while `indexer-worker` delegates to the same
  library runtime.
- `sdk/deposits` and `apps/api` own the separate PS database, retry window,
  event mirror, projection cursor, and reconciliation cases.
- HTTP reconciliation is authoritative; WebSocket `newHeads` messages are
  wake-up hints only.
- One RocksDB owner replaces distributed leasing only for v1. It does not
  authorize multiple independent writers or claim high availability.

## Ethereum Payment Service v1 selection

[`docs/PAYMENT_SERVICE.md`](./docs/PAYMENT_SERVICE.md) records the first
concrete PS selection without weakening the ownership rules above:

- `apps/api` exclusively owns one PS RocksDB path, one Ethereum `IndexScope`,
  one numeric EVM chain ID, one active policy identity, and one IX feed;
- `sdk/deposits` owns durable users, jobs, command idempotency, deposits,
  deposit-to-observation indexes, absolute ledgers, typed reconciliation, and
  collection aggregates/legs/reservations;
- PS reaches IX and stateless WS only through semantic HTTP clients, requires
  bearer authentication for every WS connection and every non-loopback IX
  connection, and never opens IX storage or custody secret material;
- the collection executor persists the exact signed envelope before broadcast,
  checks its chain ID and configured fee ceilings, and advances durable legs
  from IX facts; and
- normal startup validates immutable owner/schema/scope/policy metadata, while
  explicit migration first creates a verified physical backup, validates
  semantic records, and rebuilds supplementary indexes before rebinding; and
- one exclusive writer is an Ethereum v1 constraint, not a claim of HA.
  Another network requires another process/database until a scope-keyed PS
  design is approved.
