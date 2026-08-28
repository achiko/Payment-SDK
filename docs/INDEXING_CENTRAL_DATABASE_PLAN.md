# Indexing & Central Database Implementation Plan

## Status

The **Indexing & Central Database** decision is Accepted. Implementation has
not started.

This document is the flat execution plan for that one decision. It introduces
no new ADR and does not approve Rust changes, SQL changes, dependency changes,
database access, migration execution, or runtime rollout.

## Execution rule

- Work on one named step at a time.
- Obtain approval before editing production Rust or SQL for that step.
- Keep its behavioral test and implementation in one reviewable diff unless a
  test-only reproduction is explicitly listed first.
- Report the exact files, focused proof, and remaining risk before advancing.
- Never run migration or cleanup SQL against a retained database without a
  separate approval naming that database and scope.
- Stop when a prerequisite is missing; do not infer a dense block position,
  recreate the shared schema, or strand an existing wallet row to make progress.

## Fixed boundaries

- One PostgreSQL database, one schema, and one process-wide pool serve every
  configured chain and asset.
- One repository handle is bound to one exact `(chain, network)` scope. Assets
  are history facts inside that scope, not repository or schema selectors.
- `sdk/indexing/postgres/migrations/` is the canonical physical location for
  the deployment-owned schema creation and ordered migration scripts. The user
  will place the current scripts there; this plan does not create, copy, or
  rewrite them.
- Physical placement does not transfer domain ownership. Indexing owns only
  checkpoint, canonical history, live-output, and rollback-journal state.
- Native SOL source guards, signed envelopes, submission attempts, request
  identity, and reconciliation state remain process-local submission concerns.
  ADR-0025 adds no outgoing-operation table to the central database, and neither
  indexing adapter may persist that state opportunistically.
- `payment_wallets` remains application-owned even if a script creating it is
  physically stored in the central migration folder. Indexing runtime code
  must not query, mutate, expose, truncate, interpret, or issue DDL for its
  custody data.
- A rescan or cleanup may affect only one explicitly selected indexing scope.
  It must preserve other scopes and every application-owned table.
- `BlockPosition` is the native RPC coordinate. `BlockHeight` is the produced-
  block count. Runtime code must never infer `position = height` as a fallback.

## Phase: Schema Baseline

- [ ] **Receive scripts** — confirm the exact files placed under
  `sdk/indexing/postgres/migrations/`; record their order and checksums without
  editing them.
- [ ] **Read effective schema** — derive the final tables, columns, constraints,
  foreign keys, and indexes produced by the complete script sequence.
- [ ] **Map logical ownership** — classify every table and statement as
  indexing-owned, application-owned, or deployment-only; flag every statement
  that touches `payment_wallets` for application-owner review.
- [ ] **Bind schema identity** — confirm the scripts work under one explicit
  validated schema name and define how every pooled connection pins that schema
  so URL `search_path` settings cannot redirect unqualified SQL.
- [ ] **Cross-check adapter SQL** — compare the effective schema with every
  query and row decoder in `sdk/indexing/postgres/src/` and every database test.
- [ ] **Separate fresh and retained paths** — document which scripts create a
  fresh database and which scripts evolve an already populated database;
  baseline creation scripts must never be replayed over retained tables.
- [ ] **Name the migration executor** — record the deployment-owned tool,
  ordered apply command, and applied-version/checksum evidence. `apps/api` and
  the indexing runtime never become an implicit migration runner.
- [ ] **Define preservation proof** — specify scope row counts, indexing-row
  hashes, and non-secret `payment_wallets` sentinels to check before and after a
  transition. Secret values are neither selected for reports nor logged.
- [ ] **Define restore proof** — require a tested restore point before any
  retained-database migration; creating a backup alone is insufficient.
- [ ] **Own the test database** — replace optional, silently skipped PostgreSQL
  evidence with an isolated disposable database or schema that the test owns.
- [ ] **Validate startup compatibility** — add a read-only schema validator in
  `sdk/indexing/postgres`; application startup reports missing or incompatible
  schema and never applies implicit DDL.

The next approval boundary is **Receive scripts**. It is a read-only review and
can begin after the user says the files are in place.

## Phase: Adapter Safety

These repairs do not require the block-position schema expansion and should be
completed before that larger cutover.

- [ ] **Reject zero pool size** — add a focused regression and return a typed
  invalid-request error before deadpool can create a pool that never yields a
  connection.
- [ ] **Expose wrong-address spend** — add required-spend and tracked-spend
  regressions proving that an `OutputKey` with the wrong address cannot remove
  another address's live output.
- [ ] **Qualify spend keys** — include address in the unnested spend input and
  in the SQL match alongside transaction ID and output index.
- [ ] **Expose duplicate output identity** — add a regression where one
  `OutputId` is supplied under different addresses in the same block.
- [ ] **Reject duplicate output identity** — fail the invalid block in domain
  validation before PostgreSQL reports a unique-key conflict.
- [ ] **Expose first-commit race** — add a deterministic concurrent test in
  which two transactions attempt the first commit for one empty scope.
- [ ] **Serialize scope writes** — take one transaction-scoped PostgreSQL
  advisory lock derived from exact `(chain, network)` before checkpoint reads in
  both add and remove paths. No lock table or schema change is needed.
- [ ] **Expose page snapshot drift** — reproduce a checkpoint change between
  page queries under the default `READ COMMITTED` behavior.
- [ ] **Stabilize history pages** — execute the complete checkpoint-bound
  history page inside one read-only `REPEATABLE READ` transaction.
- [ ] **Stabilize output pages** — apply the same snapshot boundary to live-
  output pagination and cursor validation.
- [ ] **Remove global benchmark reset** — delete benchmark `TRUNCATE` behavior;
  give each run a unique scope and clean up only that scope in dependency-safe
  order.
- [ ] **Prove shared-pool isolation** — use one pool with at least two exact
  scopes; prove cross-scope handle rejection and unchanged rows in the other
  scope.

## Phase: Block Coordinates & Persistence

- [ ] **Add coordinate vocabulary** — add `BlockPosition` and atomic
  `BlockParent(position, hash)` values in `sdk/chains/base` without changing
  `BlockRef`; prove the additive base API first.
- [ ] **Review expansion SQL** — after the user's scripts are present, propose
  a separate, approval-gated migration that adds only the required nullable
  position columns. Do not edit an accepted baseline script in place.
- [ ] **Specify checkpoint expansion** — verify that nullable `position` and
  `parent_position` are the only new `checkpoint` columns.
- [ ] **Specify history expansion** — verify that nullable `block_position` and
  `block_parent_position` are the only new `history` columns.
- [ ] **Specify journal expansion** — verify the four nullable current/previous
  checkpoint position and parent-position columns required by `journal`.
- [ ] **Apply expansion atomically** — add all eight nullable columns across the
  three tables in one transactional migration in the disposable test database;
  never expose a partly expanded schema.
- [ ] **Leave unrelated tables unchanged** — prove that `movement`, `output`,
  `journal_output`, and `payment_wallets` require no coordinate column.
- [ ] **Build scoped backfill** — prepare an explicit-scope transition that
  derives dense positions only for verified Bitcoin/Ethereum rows; abort on an
  unknown or unverified populated scope.
- [ ] **Declare the writer barrier** — specify how every old indexing writer is
  stopped and prevented from restarting; old and position-aware writers must
  never overlap during the final transition.
- [ ] **Run the final delta backfill** — after the writer barrier, fill and
  verify rows created since the initial scoped backfill before constraints or a
  new writer are admitted.
- [ ] **Prove backfill preservation** — verify counts, hashes, atomic parent
  pairs, and application sentinels in an isolated restored copy.
- [ ] **Cut over complete block references atomically** — in one
  workspace-compiling and repository-working slice, require `position`,
  `height`, `hash`, optional atomic parent, and timestamp in every constructor,
  fixture, and dense Bitcoin/Ethereum adapter; update redb records/keys plus all
  PostgreSQL checkpoint, history, journal, add, remove, query, and row-decoding
  paths at the same time. Reject old redb records rather than providing a
  compatibility reader.
- [ ] **Prove repository cutover** — round-trip sparse positions and dense
  heights, exact parents, history, outputs, add, remove, restart, and old-record
  rejection through both repositories before advancing.
- [ ] **Enforce transition invariants** — accept sparse position growth only
  when produced height increments by one and the child parent position/hash
  exactly equals the checkpoint; add each rejection case separately.
- [ ] **Validate coordinate constraints** — reject null current positions,
  half-present parents, and incomplete previous checkpoints after every
  retained row has passed backfill validation.
- [ ] **Remove nullable transition state** — set required position columns
  `NOT NULL` only after the disposable migration and position-aware repository
  suite pass; do not add a runtime height-to-position fallback.
- [ ] **Record roll-forward recovery** — after final constraints, prohibit an
  old binary from restarting because it would write null positions. Recovery
  defaults to the validated new binary; relaxing constraints requires a
  separate explicit database approval and writer barrier.

The complete block-reference cutover is the only intentionally broad compile
slice in this phase. Splitting its domain value, producers, redb format, or
PostgreSQL readers/writers would require invalid placeholder coordinates or
leave a runtime-broken repository.

## Phase: Sparse Indexing & Wallet Handoff

- [ ] **Specify source contract tests** — cover complete tip, inclusive native-
  position ranges, positive returned-block limits, ordering, uniqueness, and a
  complete canonical reference at one position.
- [ ] **Cut over source contract** — replace height-addressed `block_at` and
  `canonical_hash` together with all test doubles and dense Bitcoin/Ethereum
  implementations so the workspace remains compilable.
- [ ] **Prove dense adapters** — demonstrate `position == height` only at the
  Bitcoin/Ethereum protocol boundary and return complete parent references.
- [ ] **Prove sparse synchronization** — add generic fixtures for positions
  `100 -> 103 -> 107`, produced heights `50 -> 51 -> 52`, skipped birthdays,
  restart, retained reorg, and deep-reorg failure.
- [ ] **Rewrite synchronization** — fetch actual produced blocks after the
  checkpoint position, use source-returned parents, and query remote canonical
  state by stored position while retaining produced-height ordering.
- [ ] **Rename birthdays** — replace `start_height` with `start_position` in
  filters, wallet import, storage, and activation; use checked successors and
  reject overflow. This step excludes application-owned
  `payment_wallets.start_height`, which can change only with the later approved
  application handoff and custody transition.
- [ ] **Add runtime-neutral wakeups** — use `futures_channel::oneshot` waiters
  behind the coordinator rather than making generic indexing own Tokio. Never
  hold the coordinator or wallet lock across `.await`.
- [ ] **Add coordinator race tests** — prove both wallet-publication/commit
  orders, revision invalidation, cancellation recovery, checkpoint reload, and
  lock ordering before implementation.
- [ ] **Implement the coordinator** — keep checkpoint snapshot, filter revision,
  and commit permit in `sdk/indexing`; repository I/O occurs outside its short
  critical section.
- [ ] **Inventory retained wallet rows** — determine which existing
  `payment_wallets` rows must restore operational wallets. Do not inspect or
  print secret values during the inventory.
- [ ] **Approve the application owner** — identify the application/custody
  component responsible for restart reads. The indexing decision does not
  authorize inventing a new plaintext custody adapter.
- [ ] **Implement restoration first** — load every required row through the
  approved application-owned path using the shared pool; prove no row or secret
  bytes are rewritten, returned, or logged.
- [ ] **Remove registry coupling second** — only after restoration evidence,
  remove indexing registry exports/queries and wallet `adopt`/`restore`
  coupling while preserving the physical table.
- [ ] **Break old cursors explicitly** — require position and atomic parent in
  checkpoint cursors and public block JSON; reject old height-only cursors
  rather than guessing a position.

## Phase: Composition & Release Evidence

This phase is deliberately last. **Solana Runtime Composition** is Accepted but
unimplemented. Its crate, dependencies, top-level PostgreSQL configuration,
startup sequencing, and application composition remain subject to their
explicit implementation-step approvals and prerequisites.

- [x] **Accept runtime composition** — ADR-0027 now fixes the runtime contract;
  this completed design prerequisite is not source-change authorization.
- [ ] **Open one application pool** — replace per-chain redb paths with one
  validated PostgreSQL pool only in its approved source step.
- [ ] **Construct scope handles** — create the existing Bitcoin and Ethereum
  repositories per `(chain, network)` by cloning the pool; reuse the Ethereum
  scope for native ETH and configured ERC-20 assets. The Solana implementation
  plan later extends this proven pattern with its own scope handle.
- [ ] **Prove central coexistence** — run Bitcoin and Ethereum scopes plus
  native/token asset views through one schema and pool with no cross-scope
  writes and no `payment_wallets` mutation.
- [ ] **Add the Solana source** — link the completed native source steps from
  `docs/SOLANA_IMPLEMENTATION_PLAN.md`; that plan owns the implementation and
  deterministic RPC doubles, so this central plan does not create a second
  source or contact a public cluster.
- [ ] **Prove Solana edge cases** — link the completed source/interpreter
  evidence from `docs/SOLANA_IMPLEMENTATION_PLAN.md` for skipped slots, pruning
  sandwiches, sparse parents, unavailable blocks, unsupported transaction
  versions, deadlines, call budgets, cancellation, restart, and retained
  reorgs.
- [ ] **Update evidence honestly** — move an item from "Accepted but not
  implemented" in `docs/FEATURE_VALIDATION.md` only when its cited source and
  tests exist.
- [ ] **Run focused gates** — execute each changed crate's locked tests and the
  PostgreSQL contract suite with no silent skip.
- [ ] **Run workspace gates** — format, check, test, strict Clippy, docs,
  design-lint, and `git diff --check`; report pre-existing failures separately.

## Explicitly blocked work

- No retained-database migration execution is approved by this plan.
- No production database, public RPC endpoint, funded key, or secret inspection
  is approved by this plan.
- `apps/api` PostgreSQL composition and Solana crate wiring require their
  explicit implementation-step approvals and all accepted prerequisites.
- Indexing registry deletion waits for an approved, proven application-owned
  `payment_wallets` restoration path.
- Public Transaction Semantics, Destination Account Acquisition, and Native SOL
  Submission are Accepted but unimplemented. Solana Runtime Composition is also
  Accepted but unimplemented.
- Native SOL submission implementation is outside this indexing plan and
  belongs in a separately approved Solana-wide implementation plan, with
  explicit source-step approval.
