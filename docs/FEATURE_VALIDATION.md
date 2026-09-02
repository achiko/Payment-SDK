# Feature validation

This file records evidence, not aspirations. The project is in active design;
an implementation is considered present only when the cited source and tests
exist in the current workspace.

## Implemented reusable capabilities

| Capability | Owner | Evidence |
|---|---|---|
| Exact chain/network/asset metadata and decimal amounts | `sdk/chains/base` | focused crate tests |
| Small signing, transaction snapshot, and ambiguity-carrier contracts | `sdk/chains/base` | focused crate tests prove ordinary errors remain ID-free, provider prose supplies no ID, and explicit typed ambiguity preserves the canonical ID |
| Bitcoin addresses, RPC, UTXO transactions, signing, indexing translation | `sdk/chains/bitcoin` | chain unit tests and deterministic stack test |
| Ethereum native/allowlisted ERC-20 balances, typed transfers, EIP-1559 signing, sender-keyed nonce coordination, exact-envelope ambiguity, and indexing translation | `sdk/chains/ethereum` | 81 chain unit tests, external adapter test, and deterministic ETH/USDC stack tests |
| Exact wallet history mapping, transaction-ambiguity preservation, truthful batch failure metadata, and bounded batch admission | `sdk/wallets` | history and conversion tests, four scoped `SendError` tests, direct 0/1/50/51 boundary tests, and a four-case competing-error table proving authored itemwise amount, lookup, and family precedence before sender invocation |
| Native block coordinates and sparse synchronization | `sdk/chains/base`, `sdk/indexing` | every `BlockRef` carries native position, produced height, hash, atomic parent, and timestamp; deterministic fixtures prove sparse `100 -> 103 -> 107` traversal, strict height increments, skipped birthdays, prefix restart, retained reorg, and `ReorgTooDeep` |
| Atomic indexing persistence | `sdk/indexing/redb` | complete-coordinate repository tests cover add/remove/reopen and legacy height-only record rejection, including pre-repository duplicate-output rejection |
| Position-aware PostgreSQL indexing repository | `sdk/indexing/postgres` | scope-bound `Blocks`, `Transactions`, `Outputs`, reusable registry, read-only configured-schema validation, typed zero-pool rejection, address-qualified spends, duplicate-output rejection, serialized add/remove, checkpoint-stable pages, sparse-coordinate add/remove/reopen, and one-pool multi-scope isolation; initializer and repository contracts run against owned schemas on pinned PostgreSQL 18.6 |
| Canonical fresh-schema initializer | `sdk/indexing/postgres/migrations`, deployment documentation | one checksum-locked `0001_init.sql` directly creates the final catalog without upgrade/backfill DDL; owned contracts verify final coordinates and constraints, exact catalog shape, registry preservation, and refusal to replay over an existing schema |
| Atomic runtime wallet/filter admission | `sdk/indexing`, `sdk/wallets` | one coordinator per scope serializes commit and publication permits without holding mutex guards across `.await`; deterministic coordinator and real-wallet fixtures prove both orderings, revision invalidation, cancellation recovery, checkpoint reload, and successor birthdays |
| Reusable wallet registry and restoration ownership | `sdk/wallets`, `sdk/indexing`, `sdk/indexing/postgres` | `Registry`, `RegisteredAddress`, and `Wallets::adopt`/`restore` remain reusable SDK capabilities; owned initializer and registry tests preserve the physical `payment_wallets` schema/content and its existing column mapping |
| Solana architecture ownership | `lint.toml`, `packages/design-lint` | `chain-solana` is mapped to an exact package/base/indexing/wallets layer consumed only by application and acceptance; `solana`/`sol` vocabulary is restricted to `apps/` and `sdk/chains/solana/`, with two reasoned line-local Alloy exceptions in Ethereum; positive/negative policy tests and the repository policy check pass |
| Native Solana chain and runtime composition | `sdk/chains/solana`, `apps/api`, `sdk/wallets`, `sdk/indexing`, `sdk/indexing/postgres` | Canonical `SOL` metadata, native-only `AssetKind`/`WalletConfig`, Base58 values, checked lamports, Ed25519 provider/wallet adapters, exact finalized balance/history, immutable System-plus-Memo envelopes, ambiguity reconciliation, bounded sparse finalized-slot indexing, public native-SOL routes/OpenAPI, closed environment-backed configuration, identity/Memo-before-storage ordering, one central pool, tracked readiness/submission tasks, and ordered ambiguity-preserving shutdown are implemented. Focused doubles and owned PostgreSQL 18.6 contracts pass. The exact Agave end-to-end run remains unavailable until the checksum-pinned archive is present. |
| Generic JSON-RPC/HTTP/crypto/storage mechanics | `packages/*` | package tests |

## Remaining system evidence

| Evidence | Current boundary |
|---|---|
| Owned Agave native-SOL stack | `solana_stack` is declared behind the explicit `solana-stack` manual feature and compiles; v3.1.14 commit and platform SHA-256 values, unsupported-platform handling, corruption rejection, isolated validator/PostgreSQL resources, the end-to-end scenario, and a retained-rollback/restart refill rehearsal are checked in. The real scenario is not claimed because the exact archive could not be acquired at a usable transfer rate. |
| CI ownership | No checked-in workflow owns the pinned validator and PostgreSQL 18.6 fixture, so `solana_stack` is a required manual integration target rather than automated CI evidence. |

## Runtime composition release evidence

Design-lint adoption on 2026-09-03 combines the original eleven SDK rules with
seventeen implemented reusable rules from local Husklet. Thirteen adopted
rules are enabled; the default scan has zero errors and 116 advisory warnings.
Four additional error rules remain explicit-review selections with 287 source
candidates, rather than being downgraded or suppressed. The SDK business source
and accepted design examples were not changed. All 194 linter tests and the full
workspace suite pass; formatting, workspace checks, strict Clippy, documentation,
and the linter MSRV check also pass. See
[`packages/design-lint/ADOPTION.md`](../packages/design-lint/ADOPTION.md) for
current per-rule status, test evidence, and the full review command.

Historical workspace evidence below predates that adoption; its zero-case and
25-test linter counts are not the current linter status.

Fresh Rust 1.91 evidence recorded on 2026-08-30:

- formatting, locked workspace all-target check, strict all-feature Clippy,
  no-dependency documentation, `git diff --check`, and the design-lint policy
  all pass;
- the complete locked workspace test suite passes with zero failures, including
  117 Solana tests, 81 Ethereum tests, 18 application acceptance tests, five
  runtime-loop tests, and the owned PostgreSQL 18.6 migration and 25-case
  repository contracts;
- design-lint's 25 unit tests pass, case generation produces zero cases, and
  the repository policy reports zero findings in every category;
- the explicit `solana_stack` checksum-manifest and corruption-rejection tests
  pass; and
- the real `solana_stack` scenario was invoked, refused the incomplete archive
  at the checksum gate, and executed no validator binary. Acquisition of the
  exact pinned archive is therefore unavailable external evidence, not a
  passing, skipped, or code-level result.

The final boundary audit confirms that `chain-solana` is consumed only by
`payment-api`, its direct workspace dependencies point to base, indexing,
wallets, and generic JSON-RPC, and existing Bitcoin/Ethereum tests pass without
depending on Solana internals. Application startup contains no DDL, the system
target uses loopback endpoints and fixed unfunded test seeds only, Solana RPC
configuration is redacted and no-retry, and OpenAPI publishes native SOL
without SPL routes or assets. The later predeployment schema consolidation is
recorded in the current persistence-coordinate gate below.

## Native Solana research closeout evidence

- Exact-toolchain checks used `rustc 1.91.0 (f8297e351 2025-10-28)` and
  `cargo 1.91.0 (ea2d97820 2025-10-10)`.
- A scratch-only graph combined the current workspace with the exact modular
  Solana dependency family. Its lockfile SHA-256 is
  `5d578ca06eb117006b5dd518220d741963a1036091b7c756d40e17eb05bfe060`.
- The combined graph contains 472 packages: 367 declare `rust-version`, 105
  omit it, and zero declare a version above Rust 1.91. The current graph keeps
  `alloy-consensus`, `alloy-eips`, and `alloy-rpc-types-eth` 1.8.3;
  `alloy-primitives` and `alloy-sol-types` 1.6.1; and redb 4.2.0.
- The exact legacy System-transfer-plus-Memo-v3 modular fixture passed 1/1.
  Locked offline workspace all-target check and strict workspace Clippy passed,
  as did 163 focused Ethereum, redb, indexing, runtime, wallet, and API tests.
- The proof used no public RPC and made no repository manifest or lockfile
  change. It proves dependency/API feasibility, not a Solana runtime.
- Repository integration now resolves 547 locked packages on Rust 1.91, exactly
  43 more than the immediately preceding 504-package lockfile. The integrated
  lockfile SHA-256 is
  `13965c12958ceb9ac5b2086e3c3dc61a55a82e812e0c995fe62fee5c1b449071`;
  the fixed direct Solana versions match the research set, the established
  Alloy/redb versions remain unchanged, and no monolithic Solana client or SDK
  was added. Locked workspace all-target compilation, strict workspace Clippy,
  and the complete workspace suite pass against the integrated graph.
- [PostgreSQL Schema Baseline](POSTGRESQL_SCHEMA_BASELINE.md) records the single
  predeployment initializer, final catalog, ownership, fresh-only deployment
  boundary, registry sentinel, and startup-validator evidence. No retained
  database migration has run.

## Current application validation

The approved architecture has one `apps/api` process. The previous wallet,
indexer, payment, deposit, accounting, and collection service surfaces have
been removed.

The wallet API integration binary contains eighteen tests. Its process-level
cases start the real application binary against loopback Bitcoin and Ethereum
RPC doubles and one owned temporary PostgreSQL database/schema:

| Behavior | Current evidence |
|---|---|
| Wallet generation/index selection | authenticated BTC, ETH, and USDC API creation with caller-owned address selection; ETH and USDC generation yields distinct addresses |
| Balance and transaction reads | incoming native and allowlisted-USDC node transactions indexed and returned through each generated wallet's selected-asset view |
| Single transfer | BTC, ETH, and USDC partial sends, node inclusion/confirmation, outgoing history, and reduced selected-asset balance |
| Bitcoin batch | two compatible transfers become one broadcast transaction and one ID, then appear in indexed history |
| Ethereum batch | two transfers submit as two IDs in request order, both become indexed, and balance reduces |
| Lifecycle | indexes become ready before serving and both runtimes shut down cleanly |
| Address selection | one per-scope admission boundary coordinates immutable filter plans, checkpoint commits, and runtime wallet publication without holding locks across async I/O |
| No internal transport | dependency/source audit shows no indexing HTTP adapter or second application |
| Restart | configured BTC and ETH wallets reopen the same databases and retain indexed history |
| Reorg | repository-level retained-reorg evidence and the combined generated-wallet Bitcoin/Ethereum canonical-reorg acceptance case pass |
| Multi-source Bitcoin batch | two wallets fund one transaction; each input witness carries its owner's public key |
| Ethereum accepted prefix | an outcome-ambiguous second submission returns HTTP 503 with the first transaction ID and `failed_index = 1` |
| Ethereum nonce coordination | a repeated-source `[A, B, A]` batch submits nonces `[A:0, B:0, A:1]` in request order |
| Ethereum whole-batch preflight | cumulative native overspend fails at the first threshold-crossing input with zero broadcasts |
| Ethereum ambiguous submission | coordinator fault-injection tests retain unknown exact envelopes across retryable failure and cancellation, then reconcile and replay byte-identical transactions |
| Mixed-chain preflight | BTC/ETH batch is rejected before either RPC double observes a broadcast |
| Mixed-asset preflight | ETH/USDC batch is rejected before the shared Ethereum RPC double observes a broadcast |
| ERC-20 fee presentation | USDC history retains the selected token movement with native ETH fee metadata |

## Explicitly absent

- deposit records or address-allocation workflows;
- accounting journals or user credits;
- collection planners, reservations, jobs, sweeps, gas sponsorship, or automatic token-wallet ETH top-up;
- outgoing payment state machines;
- durable native SOL request-idempotency, outgoing-envelope, or cross-process
  source-guard storage (ADR-0025 selects the implemented process-local model);
- durable or cross-process outgoing-envelope coordination; the Ethereum
  coordinator is process-local and requires one active writer per EOA;
- deposit/accounting/collection semantics around ordinary wallet sends;
- separate wallet or indexer processes;
- an indexing HTTP client;
- runtime/public migration commands or `V1`/`V2` compatibility DTOs; canonical
  ordered PostgreSQL migration scripts are deployment inputs, not a runtime API;
- hardware wallets, remote signers, HSM/KMS integration, or production custody;
- HA, multi-process database ownership, or production deployment claims.

## Known validation blockers

No code-level blocker remains for production shared-PostgreSQL composition.
Retained migration execution remains a separately authorized deployment action.
The pinned Agave archive is the current external system-evidence blocker.

## Final gates

Before a handoff, record fresh results for:

```text
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked --workspace --no-deps
cargo run --locked -p design-lint -- --policy lint.toml check .
git diff --check
```

If a gate fails, document the exact failure rather than weakening a lint or
describing the workspace as complete.

Latest persistence-coordinate gate recorded on 2026-09-01: the uninterrupted
locked workspace suite passes, including owned PostgreSQL 18 initializer
contracts 3/3, repository contracts 25/25, and application acceptance 18/18.
Formatting, locked workspace all-target compilation, strict
all-target/all-feature Clippy, no-deps documentation, design-lint, and diff
checks pass. The owned proof locks the single initializer checksum and verifies
the final catalog, constraints, scope isolation, registry sentinel, and replay
rejection; no retained database was contacted or migrated.
