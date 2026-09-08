# Indexing & Central Database Implementation Plan

## Status

**Superseded.** The [Native Solana Master Implementation Plan](SOLANA_MASTER_IMPLEMENTATION_PLAN.md)
is the only active plan for the Native Solana and central-indexing
implementation. Everything below is retained as historical, non-executable
planning evidence; its checkboxes, ordering, and approvals MUST NOT guide
implementation.

## Historical content (non-executable)

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

- [x] **Receive scripts** — confirm the exact files placed under
  `sdk/indexing/postgres/migrations/`; record their order and checksums without
  editing them.
- [x] **Read effective schema** — derive the final tables, columns, constraints,
  foreign keys, and indexes produced by the complete script sequence.
- [x] **Map logical ownership** — classify every table and statement as
  indexing-owned, application-owned, or deployment-only; flag every statement
  that touches `payment_wallets` for application-owner review.
- [x] **Bind schema identity** — confirm the scripts work under one explicit
  validated schema name and define how every pooled connection pins that schema
  so URL `search_path` settings cannot redirect unqualified SQL.
- [x] **Cross-check adapter SQL** — compare the effective schema with every
  query and row decoder in `sdk/indexing/postgres/src/` and every database test.
- [x] **Separate fresh and retained paths** — document which scripts create a
  fresh database and which scripts evolve an already populated database;
  baseline creation scripts must never be replayed over retained tables.
- [x] **Name the migration executor** — record the deployment-owned tool,
  ordered apply command, and applied-version/checksum evidence. `apps/api` and
  the indexing runtime never become an implicit migration runner.
- [x] **Define preservation proof** — specify scope row counts, indexing-row
  hashes, and non-secret `payment_wallets` sentinels to check before and after a
  transition. Secret values are neither selected for reports nor logged.
- [x] **Define restore proof** — require a tested restore point before any
  retained-database migration; creating a backup alone is insufficient.
- [x] **Own the test database** — replace optional, silently skipped PostgreSQL
  evidence with an isolated disposable database or schema that the test owns.
- [x] **Validate startup compatibility** — add a read-only schema validator in
  `sdk/indexing/postgres`; application startup reports missing or incompatible
  schema and never applies implicit DDL.

The schema baseline and retained-transition runbook are complete. Retained
execution remains a separately authorized operational event; no retained
database was contacted by this implementation phase.

## Phase: Adapter Safety

These repairs do not require the block-position schema expansion and should be
completed before that larger cutover.

- [x] **Reject zero pool size** — add a focused regression and return a typed
  invalid-request error before deadpool can create a pool that never yields a
  connection.
- [x] **Expose wrong-address spend** — add required-spend and tracked-spend
  regressions proving that an `OutputKey` with the wrong address cannot remove
  another address's live output.
- [x] **Qualify spend keys** — include address in the unnested spend input and
  in the SQL match alongside transaction ID and output index.
- [x] **Expose duplicate output identity** — add a regression where one
  `OutputId` is supplied under different addresses in the same block.
- [x] **Reject duplicate output identity** — fail the invalid block in domain
  validation before PostgreSQL reports a unique-key conflict.
- [x] **Expose first-commit race** — add a deterministic concurrent test in
  which two transactions attempt the first commit for one empty scope.
- [x] **Serialize scope writes** — take one transaction-scoped PostgreSQL
  advisory lock derived from exact `(chain, network)` before checkpoint reads in
  both add and remove paths. No lock table or schema change is needed.
- [x] **Expose page snapshot drift** — reproduce a checkpoint change between
  page queries under the default `READ COMMITTED` behavior.
- [x] **Stabilize history pages** — execute the complete checkpoint-bound
  history page inside one read-only `REPEATABLE READ` transaction.
- [x] **Stabilize output pages** — apply the same snapshot boundary to live-
  output pagination and cursor validation.
- [x] **Remove global benchmark reset** — delete benchmark `TRUNCATE` behavior;
  give each run a unique scope and clean up only that scope in dependency-safe
  order.
- [x] **Prove shared-pool isolation** — use one pool with at least two exact
  scopes; prove cross-scope handle rejection and unchanged rows in the other
  scope.

## Phase: Block Coordinates & Persistence

- [x] **Add coordinate vocabulary** — add `BlockPosition` and atomic
  `BlockParent(position, hash)` values in `sdk/chains/base` without changing
  `BlockRef`; prove the additive base API first.
- [x] **Review expansion SQL** — after the user's scripts are present, propose
  a separate, approval-gated migration that adds only the required nullable
  position columns. Do not edit an accepted baseline script in place.
- [x] **Specify checkpoint expansion** — verify that nullable `position` and
  `parent_position` are the only new `checkpoint` columns.
- [x] **Specify history expansion** — verify that nullable `block_position` and
  `block_parent_position` are the only new `history` columns.
- [x] **Specify journal expansion** — verify the four nullable current/previous
  checkpoint position and parent-position columns required by `journal`.
- [x] **Apply expansion atomically** — add all eight nullable columns across the
  three tables in one transactional migration in the disposable test database;
  never expose a partly expanded schema.
- [x] **Leave unrelated tables unchanged** — prove that `movement`, `output`,
  `journal_output`, and `payment_wallets` require no coordinate column.
- [x] **Build scoped backfill** — prepare an explicit-scope transition that
  derives dense positions only for verified Bitcoin/Ethereum rows; abort on an
  unknown or unverified populated scope.
- [x] **Declare the writer barrier** — specify how every old indexing writer is
  stopped and prevented from restarting; old and position-aware writers must
  never overlap during the final transition.
- [x] **Run the final delta backfill** — after the writer barrier, fill and
  verify rows created since the initial scoped backfill before constraints or a
  new writer are admitted.
- [x] **Prove backfill preservation** — verify counts, hashes, atomic parent
  pairs, and application sentinels in an isolated restored copy.
- [x] **Cut over complete block references atomically** — in one
  workspace-compiling and repository-working slice, require `position`,
  `height`, `hash`, optional atomic parent, and timestamp in every constructor,
  fixture, and dense Bitcoin/Ethereum adapter; update redb records/keys plus all
  PostgreSQL checkpoint, history, journal, add, remove, query, and row-decoding
  paths at the same time. Reject old redb records rather than providing a
  compatibility reader.
- [x] **Prove repository cutover** — round-trip sparse positions and dense
  heights, exact parents, history, outputs, add, remove, restart, and old-record
  rejection through both repositories before advancing.
- [x] **Enforce transition invariants** — accept sparse position growth only
  when produced height increments by one and the child parent position/hash
  exactly equals the checkpoint; add each rejection case separately.
- [x] **Validate coordinate constraints** — reject null current positions,
  half-present parents, and incomplete previous checkpoints after every
  retained row has passed backfill validation.
- [x] **Remove nullable transition state** — set required position columns
  `NOT NULL` only after the disposable migration and position-aware repository
  suite pass; do not add a runtime height-to-position fallback.
- [x] **Record roll-forward recovery** — after final constraints, prohibit an
  old binary from restarting because it would write null positions. Recovery
  defaults to the validated new binary; relaxing constraints requires a
  separate explicit database approval and writer barrier.

The complete block-reference cutover is the only intentionally broad compile
slice in this phase. Splitting its domain value, producers, redb format, or
PostgreSQL readers/writers would require invalid placeholder coordinates or
leave a runtime-broken repository.

## Phase: Sparse Indexing & Wallet Handoff

- [x] **Specify source contract tests** — cover complete tip, inclusive native-
  position ranges, positive returned-block limits, ordering, uniqueness, and a
  complete canonical reference at one position.
- [x] **Cut over source contract** — replace height-addressed `block_at` and
  `canonical_hash` together with all test doubles and dense Bitcoin/Ethereum
  implementations so the workspace remains compilable.
- [x] **Prove dense adapters** — demonstrate `position == height` only at the
  Bitcoin/Ethereum protocol boundary and return complete parent references.
- [x] **Prove sparse synchronization** — add generic fixtures for positions
  `100 -> 103 -> 107`, produced heights `50 -> 51 -> 52`, skipped birthdays,
  restart, retained reorg, and deep-reorg failure.
- [x] **Rewrite synchronization** — fetch actual produced blocks after the
  checkpoint position, use source-returned parents, and query remote canonical
  state by stored position while retaining produced-height ordering.
- [x] **Rename birthdays** — replace `start_height` with `start_position` in
  filters, wallet import, storage, and activation; use checked successors and
  reject overflow. The physical `payment_wallets.start_height` column remains
  unchanged as the existing reusable registry adapter's encoding boundary.
- [x] **Add runtime-neutral wakeups** — use `futures_channel::oneshot` waiters
  behind the coordinator rather than making generic indexing own Tokio. Never
  hold the coordinator or wallet lock across `.await`.
- [x] **Add coordinator race tests** — prove both wallet-publication/commit
  orders, revision invalidation, cancellation recovery, checkpoint reload, and
  lock ordering before implementation.
- [x] **Implement the coordinator** — keep checkpoint snapshot, filter revision,
  and commit permit in `sdk/indexing`; repository I/O occurs outside its short
  critical section.
- [x] **Preserve reusable wallet ownership** — keep `Registry`,
  `RegisteredAddress`, `Wallets::adopt`/`restore`, and custody/persistence
  capabilities in the reusable SDK; do not move them exclusively into
  `apps/api` or remove indexing registry support.
- [x] **Prove wallet-table preservation** — migration and repository tests keep
  `payment_wallets` schema and sentinel content unchanged; its physical
  `start_height` column remains the approved registry encoding boundary.
- [x] **Break old cursors explicitly** — require position and atomic parent in
  checkpoint cursors and public block JSON; reject old height-only cursors
  rather than guessing a position.

## Phase: Composition & Release Evidence

This phase is implemented. **Solana Runtime Composition** extends the same
top-level PostgreSQL configuration, startup sequencing, and application
composition without changing reusable SDK registry ownership.

- [x] **Accept runtime composition** — ADR-0027 now fixes the runtime contract;
  this completed design prerequisite is not source-change authorization.
- [x] **Open one application pool** — replace per-chain redb paths with one
  validated PostgreSQL pool only in its approved source step.
- [x] **Construct scope handles** — create the existing Bitcoin and Ethereum
  repositories per `(chain, network)` by cloning the pool; reuse the Ethereum
  scope for native ETH and configured ERC-20 assets. The Solana implementation
  plan later extends this proven pattern with its own scope handle.
- [x] **Prove central coexistence** — run Bitcoin and Ethereum scopes plus
  native/token asset views through one schema and pool with no cross-scope
  writes and no `payment_wallets` mutation.
- [x] **Add the Solana source** — link the completed native source steps from
  `docs/SOLANA_IMPLEMENTATION_PLAN.md`; that plan owns the implementation and
  deterministic RPC doubles, so this central plan does not create a second
  source or contact a public cluster.
- [x] **Prove Solana edge cases** — link the completed source/interpreter
  evidence from `docs/SOLANA_IMPLEMENTATION_PLAN.md` for skipped slots, pruning
  sandwiches, sparse parents, unavailable blocks, unsupported transaction
  versions, deadlines, call budgets, cancellation, restart, and retained
  reorgs.
- [x] **Update evidence honestly** — move an item from "Accepted but not
  implemented" in `docs/FEATURE_VALIDATION.md` only when its cited source and
  tests exist.
- [x] **Run focused gates** — execute each changed crate's locked tests and the
  PostgreSQL contract suite with no silent skip.
- [x] **Run workspace gates** — format, check, test, strict Clippy, docs,
  design-lint, and `git diff --check`; report pre-existing failures separately.

## Explicitly blocked work

- No retained-database migration execution is approved by this plan.
- No production database, public RPC endpoint, funded key, or secret inspection
  is approved by this plan.
- `apps/api` PostgreSQL composition and Solana crate wiring are implemented;
  retained-database migration and deployment still require separate authority.
- Indexing registry deletion and moving restoration exclusively into `apps/api`
  are outside the accepted design; the reusable SDK path remains supported.
- Public Transaction Semantics, Destination Account Acquisition, Native SOL
  Submission, and Solana Runtime Composition are implemented.
- Native SOL submission implementation is outside this indexing plan and
  belongs in a separately approved Solana-wide implementation plan, with
  explicit source-step approval.
