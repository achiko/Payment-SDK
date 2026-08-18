# Payment SDK

A Rust workspace for composing chain-native wallets, durable transaction
observation, and payment workflows behind small protocol-neutral contracts.

## Current workspace

- `sdk/chains/base` contains the approved neutral address, network, asset,
  decimal, block, signing, and transaction boundaries.
- `sdk/chains/bitcoin` and `sdk/chains/ethereum` own every protocol-specific
  address, RPC, transaction, signing, and indexing rule.
- `sdk/wallets` provides `Wallet`, its small capabilities, provider selection,
  and the `Wallets` composition collection.
- `sdk/indexing` provides synchronization and the consumer-facing `Watcher`,
  `History`, `Observer`, and `Indexer` contracts.
- `sdk/indexing/rocksdb` implements indexing persistence; `sdk/indexing/http`
  implements the chain-neutral remote consumer.
- `apps/indexer` is the runnable Bitcoin/Ethereum indexer composition root.
- `apps/wallet` is a runnable stateless HTTP composition root for configured
  Bitcoin/Ethereum wallets; with no chain variables it is live but not ready.
- `apps/api` provides durable orchestration plus a configured payment binary
  that composes wallets, remote indexing, RocksDB, authenticated HTTP,
  supervised reconciliation, and one optional finite BTC-native, ETH-native,
  or ERC-20 deposit-key set with server-derived collection planning.
- `packages/*` contains generic crypto, HTTP, JSON-RPC, storage, and design-lint
  infrastructure and may not import SDK code.

The current workspace has no production custody process. The optional local
deposit resolver keeps explicitly configured keys in-process and is suitable
only where that custody policy is accepted. TLS termination remains a
deployment responsibility. Do not infer production readiness from public
traits or a successful build.

## Core flow

```text
wallet.transaction()
  -> chain-native builder
  -> prepare() returns durable exact signed bytes
  -> persist SignedTransaction
  -> register transaction watch
  -> broadcast the same bytes
  -> reconcile confirmation and reorg events through Indexer
```

Wallets never poll for finality. Indexing is the source of transaction history,
confirmation, and reorg corrections. Bitcoin UTXO selection consumes the
generic snapshot-consistent output query but retains Bitcoin validation in the
Bitcoin crate.

## Documentation

- [`ARCHITECTURE.md`](./ARCHITECTURE.md): ownership and dependency rules
- [`docs/CONTRACTS.md`](./docs/CONTRACTS.md): current public API composition
- [`docs/SYSTEM_REQUIREMENTS.md`](./docs/SYSTEM_REQUIREMENTS.md): canonical
  requirements and open decisions
- [`docs/INDEXING.md`](./docs/INDEXING.md): indexing model
- [`docs/refactoring.md`](./docs/refactoring.md): detailed refactoring design
- [`docs/CHAIN_RESEARCH.md`](./docs/CHAIN_RESEARCH.md): cross-chain research

## Validation

Use locked dependencies. On this macOS workspace the `mac` wrapper runs host
Cargo:

```bash
mac cargo fmt --all -- --check
mac cargo check --locked --workspace --all-targets
mac cargo test --locked --workspace
mac cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
mac cargo doc --locked --workspace --no-deps
```

The deterministic indexer runtime tests run loopback mock Bitcoin/Ethereum RPC,
the actual HTTP runtime, and temporary RocksDB storage. They do not broadcast
funds or prove production node compatibility.

The cross-service Ethereum acceptance test additionally composes payment HTTP,
the concrete wallet and signer, RPC broadcast, Indexer HTTP, reconciliation,
restart recovery, and a canonical reorg correction over real temporary RocksDB
databases:

```bash
mac cargo test --locked -p system-tests --test ethereum_payment
```
