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
| Ethereum native/allowlisted ERC-20 balances, typed transfers, EIP-1559 signing, sender-keyed nonce coordination, and indexing translation | `sdk/chains/ethereum` | 74 chain unit tests, external adapter test, and deterministic ETH/USDC stack tests |
| Exact wallet history mapping | `sdk/wallets` | history tests |
| Reorg-safe filtered indexing contracts and synchronizer | `sdk/indexing` | synchronizer and contract tests |
| Atomic indexing persistence | `sdk/indexing/redb` | repository tests |
| Current height-only PostgreSQL indexing repository | `sdk/indexing/postgres` | scope-bound `Blocks`, `Transactions`, and `Outputs`; database contract tests run when `POSTGRES_TEST_URL` is configured |
| Generic JSON-RPC/HTTP/crypto/storage mechanics | `packages/*` | package tests |

## Accepted but not implemented

| Decision | Accepted source | Missing implementation evidence |
|---|---|---|
| Provider-owned native wallet generation | Provider Generation ADR | current `Provider::generate` still has a generic secp256k1 default; Bitcoin and Ethereum do not yet implement mandatory native `generate`, and no Ed25519 Solana provider exists |
| Canonical plain Base58 and native SOL wallet values | Canonical Base58 ADR and Solana requirements | public and wallet address-encoding enums have no plain Base58 variant; no Solana crate, Ed25519 seed provider, checked lamport value, or exact finalized SOL balance adapter exists |
| Native SOL block interpretation and canonical history | Native SOL History and Indexing & Central Database ADRs | no Solana `BlockInterpreter` or tests for legacy/version-0 loaded addresses, top-level/inner System transfers, exact fee/status, failed-transaction movement suppression, completeness shielding, or empty UTXO changes exist |
| Public batch bounds, occurrence identity, deterministic error precedence, and locally derived ambiguous IDs | Public Transaction Semantics ADR | current code has no `InvalidBatch`, 50-item SDK/OpenAPI/defensive-sender bounds, pre-conversion oversized guard, query-before-JSON rejection, optional domain failed index, or base/wallet/HTTP ambiguous-ID propagation; complete duplicate/alias/original-index evidence is also missing |
| Native SOL source/destination acquisition and closing-witness floor | Destination Account Acquisition ADR | no Solana crate, client, coordinator, account DTO, stable deduplication, exact one-call mapping, atomic `U` handoff, response-bound, cancellation, malformed-payload, floor-publication, or lexical-lease-release tests exist |
| Native SOL construction, submission, exact-byte replay, and ambiguity reconciliation | Native SOL Submission ADR | no Solana transaction/message/Memo-v3 builder, executable-Memo readiness probe, recent-blockhash lifetime, fee/simulation RPC, source coordinator, exact-signature broadcaster, three-call replay bound, checkpoint-stable indexed absence proof, background reconciliation, ambiguous-ID projection, or duplicate-risk tests exist |
| Native block position separated from produced height | Block Position and Indexing & Central Database ADRs | current `BlockRef`, synchronization, birthdays, redb records, PostgreSQL rows, and cursors remain height-only |
| One central PostgreSQL database/schema/pool with scope-bound repositories | Indexing & Central Database ADR and canonical architecture | the scope-bound height-only adapter exists, but `apps/api` still composes per-chain redb; no production shared-pool, dual-coordinate, or preservation system test exists |
| Canonical schema history under `sdk/indexing/postgres/migrations/` | Indexing & Central Database ADR | three scripts are present; baseline equivalence, ordered-upgrade behavior, checksums, and table-ownership/preservation evidence remain unverified |
| Application ownership of `payment_wallets` restart reads | Indexing & Central Database ADR | `Wallets` still accepts an optional indexing `Registry` and couples registration to `adopt`/`restore`; indexing exports that surface and the PostgreSQL adapter still queries the table |
| Atomic runtime wallet/filter admission against checkpoint movement | Indexing & Central Database ADR | no per-scope admission coordinator prevents a sync pass from advancing beyond a newly generated wallet's birthday before that wallet enters the authoritative filter snapshot |
| Sparse finalized Solana traversal and retained-reorg evidence | Indexing & Central Database ADR | no Solana source or deterministic sparse-slot test exists; accepted runtime/crate composition is also unimplemented |
| Solana crate and runtime composition | Solana Runtime Composition ADR | no `chain-solana` crate, selected modular dependencies, Rust 1.85/Alloy reconciliation proof, singular no-retry RPC config/redaction, genesis/Memo startup probes, shared-pool application composition, tracked submission supervisor, readiness regression/fatal behavior, shutdown-race/indefinite-ambiguity evidence, pinned checksummed validator fixture, or explicit `solana_stack` test target exists |

## Current application validation

The approved architecture has one `apps/api` process. The previous wallet,
indexer, payment, deposit, accounting, and collection service surfaces have
been removed.

The wallet API integration binary contains fourteen tests. Its process-level
cases start the real application binary against loopback Bitcoin and Ethereum
RPC doubles and temporary redb files:

| Behavior | Current evidence |
|---|---|
| Wallet generation/index selection | authenticated BTC, ETH, and USDC API creation with caller-owned address selection; ETH and USDC generation yields distinct addresses |
| Balance and transaction reads | incoming native and allowlisted-USDC node transactions indexed and returned through each generated wallet's selected-asset view |
| Single transfer | BTC, ETH, and USDC partial sends, node inclusion/confirmation, outgoing history, and reduced selected-asset balance |
| Bitcoin batch | two compatible transfers become one broadcast transaction and one ID, then appear in indexed history |
| Ethereum batch | two transfers submit as two IDs in request order, both become indexed, and balance reduces |
| Lifecycle | indexes become ready before serving and both runtimes shut down cleanly |
| Address selection | caller registry snapshots are supplied to each synchronization run |
| No internal transport | dependency/source audit shows no indexing HTTP adapter or second application |
| Restart | configured BTC and ETH wallets reopen the same databases and retain indexed history |
| Reorg | repository-level reorg evidence passes; the combined generated-wallet acceptance case is currently blocked before reorg by the runtime filter-registration race recorded below |
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
  source-guard storage (ADR-0025 accepts an unimplemented process-local model);
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

- Runtime wallet generation is not admitted atomically against an in-flight
  indexing pass. A pass can capture the old address-filter snapshot, observe
  new blocks after `POST /v1/wallets` returns, and advance the checkpoint
  without the newly generated address. The combined Bitcoin/Ethereum reorg
  acceptance test currently exposes this pre-existing race before its reorg
  assertions. Fixing it requires a registration/synchronization admission
  boundary; a timing delay would not be valid evidence.
- Full workspace strict Clippy currently stops on pre-existing
  `collapsible_if` findings in unchanged `packages/http` and
  `packages/design-lint` source. Focused strict Clippy for the Phase 6 crates
  passes.

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

Latest Phase 6 handoff: formatting, workspace check, documentation,
design-lint tests/case generation/policy check, diff check, and every workspace
test outside `payment-api` pass. In `payment-api`, 21 tests pass when the
isolated reorg case above is skipped. The complete workspace test and Clippy
gates remain blocked exactly as recorded in this document.
