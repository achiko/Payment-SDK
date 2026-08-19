# Feature validation

This file records evidence, not aspirations. The project is in active design;
an implementation is considered present only when the cited source and tests
exist in the current workspace.

## Implemented reusable capabilities

| Capability | Owner | Evidence |
|---|---|---|
| Exact chain/network/asset metadata and decimal amounts | `sdk/chains/base` | focused crate tests |
| Small signing and transaction snapshot contracts | `sdk/chains/base` | focused crate tests |
| Bitcoin addresses, RPC, UTXO transactions, signing, indexing translation | `sdk/chains/bitcoin` | chain unit tests and deterministic stack test |
| Ethereum addresses, RPC, EIP-1559 transactions, signing, indexing translation | `sdk/chains/ethereum` | chain unit tests and deterministic stack test |
| Provider-selected wallet generation/import | `sdk/wallets` | provider/registry tests |
| Exact wallet history mapping | `sdk/wallets` | history tests |
| Reorg-safe filtered indexing contracts and synchronizer | `sdk/indexing` | synchronizer and contract tests |
| Atomic indexing persistence | `sdk/indexing/rocksdb` | repository tests |
| Generic JSON-RPC/HTTP/crypto/storage mechanics | `packages/*` | package tests |

## Current application validation

The approved architecture has one `apps/api` process. The previous wallet,
indexer, payment, deposit, accounting, and collection service surfaces have
been removed.

The deterministic one-process TCP acceptance suite contains seven tests. It
starts the real application binary against loopback Bitcoin and Ethereum RPC
doubles and temporary RocksDB databases:

| Behavior | Current evidence |
|---|---|
| Wallet generation/index selection | authenticated BTC and ETH API creation with caller-owned address selection |
| Balance and transaction reads | incoming node transaction indexed and returned through each generated wallet |
| Single transfer | BTC and ETH partial sends, node inclusion/confirmation, outgoing history, and reduced balance |
| Bitcoin batch | two compatible transfers become one broadcast transaction and one ID, then appear in indexed history |
| Ethereum batch | two transfers submit as two IDs in request order, both become indexed, and balance reduces |
| Lifecycle | indexes become ready before serving and both runtimes shut down cleanly |
| Address selection | caller registry snapshots are supplied to each synchronization run |
| No internal transport | dependency/source audit shows no indexing HTTP adapter or second application |
| Restart | configured BTC and ETH wallets reopen the same databases and retain indexed history |
| Reorg | same-height replacement branches remove orphan history and expose only replacement canonical history |
| Multi-source Bitcoin batch | two wallets fund one transaction; each input witness carries its owner's public key |
| Ethereum accepted prefix | a rejected second submission returns the first transaction ID and `failed_index = 1` |
| Mixed-chain preflight | BTC/ETH batch is rejected before either RPC double observes a broadcast |

## Explicitly absent

- deposit records or address-allocation workflows;
- accounting journals or user credits;
- collection planners, reservations, jobs, sweeps, or token gas workflows;
- outgoing payment state machines;
- deposit/accounting/collection semantics around ordinary wallet sends;
- separate wallet or indexer processes;
- an indexing HTTP client;
- migration commands or `V1`/`V2` compatibility DTOs;
- hardware wallets, remote signers, HSM/KMS integration, or production custody;
- HA, multi-process database ownership, or production deployment claims.

## Final gates

Before a handoff, record fresh results for:

```text
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- check .
git diff --check
```

If a gate fails, document the exact failure rather than weakening a lint or
describing the workspace as complete.
