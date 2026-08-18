# Repository guide for coding agents

## Source of truth

This file applies to the entire repository. Read it before changing code.

Use these sources in order:

1. `docs/SYSTEM_REQUIREMENTS.md` for canonical scope and acceptance criteria.
2. `ARCHITECTURE.md` for ownership and dependency rules.
3. `docs/CONTRACTS.md` for current reusable Rust boundaries.
4. `docs/FEATURE_VALIDATION.md` for evidence and honest implementation status.
5. `docs/INDEXING.md`, `docs/REQUIREMENTS.md`, and research documents for
   focused context.
6. Current code and tests for implemented behavior.

The project is pre-release and in active design. Remove superseded code
directly; do not add legacy decoders, compatibility aliases, migrations, or
`V1`/`V2` project DTOs. Protocol-defined version names are unaffected.

`old/` and `reference/` are excluded from the workspace. `reference/` contains
research checkouts, not production dependencies. Do not copy their architecture
or edit either directory unless a task explicitly targets it.

## CodeGraph

If `.codegraph/` exists at the repository root, use `codegraph explore` before
grep/find or broad file reading when locating or understanding code. If it does
not exist, skip CodeGraph; indexing the repository is the user's decision.

## Workspace ownership

| Path | Ownership |
|---|---|
| `apps/api` | Only executable; composes public HTTP, BTC/ETH wallets, embedded indexing synchronizers, and storage |
| `sdk/chains/base` | Approved chain-neutral values, signing, transaction, and broadcast capabilities |
| `sdk/chains/bitcoin` | Bitcoin-native RPC, addresses, UTXOs, transactions, wallets, and indexing |
| `sdk/chains/ethereum` | Ethereum-native RPC, addresses, transactions, wallets, and indexing |
| `sdk/wallets` | Chain-neutral wallet capabilities, providers, history, batch sending, and `Wallets` composition |
| `sdk/indexing` | Chain-independent synchronization, address-watch, history, checkpoint, and repository contracts |
| `sdk/indexing/rocksdb` | RocksDB implementation of indexing repository contracts |
| `packages/crypto` | Transferable cryptographic primitives without chain policy |
| `packages/http` | Generic HTTP server mechanics |
| `packages/json-rpc` | Generic JSON-RPC mechanics while retained |
| `packages/storage` | Backend-independent atomic persistence mechanics |
| `packages/storage/rocksdb` | Generic RocksDB engine adapter |
| `packages/design-lint` | Repository architecture and Rust API linter |
| `apps/api/tests` | Deterministic tests of the composed application binary |

There is no deposit/accounting/collection SDK, wallet service, indexer service,
remote indexing client, or payment workflow. Do not reintroduce those without a
new approved design.

## Architecture rules

- `apps/api` is the sole process and composition root. It directly constructs
  chain RPC clients, RocksDB repositories, synchronizers, and wallet
  providers. There are no internal HTTP hops.
- Keep application handlers dependent on wallet/indexing abstractions.
  Concrete composition belongs in startup.
- Preserve chain-native types through build, signing, broadcast, and receipt
  handling. Do not invent a universal Bitcoin/Ethereum transaction model.
- The wallet batch boundary is generic, but execution remains chain-native:
  Bitcoin may combine compatible transfers into one multi-input/output
  transaction; Ethereum submits nonce-ordered transactions. Never model this as
  deposit collection or accounting.
- Keep `sdk/chains/base` small and approved. Do not add a god `ChainService`,
  RPC interface, indexing type, or concrete-chain convenience type there.
- A concrete chain owns all its address parsing, RPC DTOs/methods, blocks,
  transactions, fee/gas/script/UTXO rules, and indexing interpretation.
- Every concrete chain uses the required `address.rs`, `batch.rs`, `error.rs`,
  and `lib.rs` files plus directory modules `indexer`, `rpc`, `transaction`, and
  `wallet`. The `chain-layout` design-lint rule enforces this ownership map.
- Indexing owns no transport. `sdk/indexing` has no Axum routes, URLs,
  authentication, or remote client.
- Chain interpreters produce semantic facts/effects, never RocksDB keys.
  Physical encoding belongs exclusively to `sdk/indexing/rocksdb`.
- `packages/*` must remain useful outside blockchain projects and must not
  import SDK/application crates.
- Prefer established protocol libraries over handwritten standards. Local
  wrappers need a clear abstraction, policy, or error-isolation purpose.
- The chain deletion test is mandatory: removing one concrete chain must leave
  base, wallets, indexing, storage, HTTP, JSON-RPC, crypto, and the other chain
  usable.

## Transaction and security invariants

The required flow is:

```text
chain-native request
  -> unsigned chain-native transaction
  -> chain computes payload/digest
  -> injected Signer signs it
  -> chain validates and inserts signature
  -> signed chain-native bytes
  -> broadcast
  -> embedded indexing observes confirmation/reorg
```

- Bitcoin owns scripts, network checks, UTXOs, dust, fees/weight, sighashes,
  witnesses, and consensus encoding.
- Ethereum owns chain ID, nonce, gas, EIP-1559 envelopes, token calldata,
  receipts, logs, and signer recovery.
- Use checked integer atomic units. Never use floating point for money.
- `base::KeyPair` is development in-process custody, not production custody.
  Never export, log, serialize, clone, or debug-print private keys.
- A concrete chain may receive `&dyn Signer`; it must not select hardware,
  remote, HSM, or KMS custody.
- RPC acceptance is submission, not confirmation. Confirmation comes from
  indexing. RPC outage does not prove a transaction was dropped.
- Do not run against a funded key or public broadcast endpoint unless the user
  explicitly requests that exact external action and reviews the transaction.

## Indexing invariants

- A checkpoint includes height and hash.
- Watches are durable, idempotent, scope-safe, and birthday-height aware.
- One atomic block commit includes effects, undo, observation revisions, and
  checkpoint movement.
- Reorgs append correction revisions and restore chain projection. Never erase
  prior observation history to make the new branch look original.
- Transactions retain multiple stable movements. Never collapse a UTXO
  transaction into a fake single `from -> to -> amount` record.
- RocksDB record structs and codecs stay private to its adapter.
- Future storage adapters must be able to implement the same semantic
  repository contracts without changing chain interpreters.

## Rust and API conventions

- Rust 2024, resolver 3, MSRV 1.85, locked dependencies.
- Workspace-wide `unsafe_code = "forbid"` must not be weakened.
- Prefer precise newtypes/enums over strings/booleans when behavior differs.
- Keep public traits to one to three cohesive methods. Do not split naturally
  paired operations merely to reach one method.
- Use short, obvious names. Within `bitcoin`, prefer `Address` over
  `BitcoinAddress`. Do not retain empty marker structs.
- Keep modules focused, public re-exports intentional, and helpers private.
- Return structured errors; preserve retryability at source boundaries.
- Use `Result`/`Option` and checked arithmetic. Avoid runtime `unwrap`/panic.
- Derive boundary DTO traits only when useful and never where secrets leak.
- Add focused unit tests beside implementations and deterministic system tests
  for composed behavior.
- Comments explain ownership/protocol invariants, not syntax.

## Change workflow

1. Inspect `git status --short`; preserve unrelated concurrent/user changes.
2. Read canonical docs plus the target manifests, APIs, implementation, tests.
3. Identify the owning layer before editing.
4. Make the smallest coherent change toward the approved architecture; delete
   stale code rather than retaining compatibility scaffolding.
5. Update docs for public contracts or ownership changes.
6. Add focused tests and run scope-appropriate validation.
7. Report pre-existing/concurrent failures separately and do not weaken gates.

## Validation

Use locked dependencies:

```bash
cargo fmt --all -- --check
git diff --check

cargo check --locked -p chain-bitcoin --all-targets
cargo test --locked -p chain-bitcoin
cargo clippy --locked -p chain-bitcoin --all-targets --no-deps -- -D warnings

cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- check .
```

For Markdown-only changes, `git diff --check` plus a link/path review is enough.
For Cargo/public-API changes, run full checks and tests. Chain transaction or
signing changes also require focused chain Clippy/tests. System tests must use
loopback RPC doubles and temporary databases, never public networks.
