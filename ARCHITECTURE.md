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
- `apps/api/` is the PS composition root and owns user/deposit orchestration.
- `apps/indexer/` is the IX composition root and owns its checkpoint/watch/observation DB.
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
- Do not select PostgreSQL, SQLite, RocksDB, memory, or another storage backend
  during this contract phase.
- Do not assume all account-oriented chains use Ethereum's nonce/value/gas
  transaction model.
- Do not name an application `payment-service`; name executables after their
  actual role, such as `api`, `worker`, or `cli`.
- Do not add a dependency from a more generic layer to a less generic layer.

## Current dependency graph

```text
apps/api
├── deposits
├── indexing boundary
├── signer boundary
└── storage boundary

apps/indexer
├── chain-bitcoin / chain-ethereum
├── indexing
└── storage

apps/wallet
├── chain-bitcoin / chain-ethereum
└── signer (no direct storage or DB backend)

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
└── json-rpc

deposits      -> indexing + chain-identity + signer
indexing      -> storage + chain-identity
chain-contract -> chain-identity + signer
signer-local  -> signer
signer-trezor -> signer + transport
json-rpc      -> transport
http          -> transport
packages/*    -> packages/* only
```
