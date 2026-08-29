# PostgreSQL Schema Baseline

## Status and evidence boundary

This schema baseline was recorded statically on 2026-08-28 for the accepted
**Indexing & Central Database** decision. It is derived from the three SQL files
under `sdk/indexing/postgres/migrations/`, the complete current PostgreSQL
adapter, its examples and repository-contract tests, ADR-0026, ADR-0003, and the
central-database plan. On 2026-08-29 the owned PostgreSQL 18.6 harness added
runtime proof of the same baseline: reviewed checksums, ordered application,
effective catalog, scope/index keys, constraints, and a preserved registry
sentinel all pass in disposable schemas.

No retained state was inspected or changed. This evidence does not prove that
any deployed database has these migrations applied; retained migration remains
a separately authorized operational action with its own restore and writer
barrier.

## Ordered canonical artifacts

The canonical order is lexical filename order. The files are immutable baseline
inputs: a later change must be a new ordered migration, not an edit to one of
these files.

| Order | File | SHA-256 | Effective change |
|---:|---|---|---|
| 1 | `0001_init.sql` | `1ca86f471b6cbe58880fcf42f4e2c433e29a0b3dc405fc1a03e517aed6bc886c` | Creates the complete height-only indexing schema and application-owned `payment_wallets` table. |
| 2 | `0002_output_pagination.sql` | `0949bfa6a51ceb8393ba879a0643512c8c6d915aa532d288623acbf55d79e6fb` | Replaces the prefix-only output-address index with the address-and-output-identity pagination index. |
| 3 | `0003_movement_cascade.sql` | `a9de19a7ede932b73463d62f9702133aad8bcd87b350f524679965c78c27a81b` | Adds the movement height index and removes the movement-to-history foreign key so reorg reversal deletes movements explicitly. |
| 4 | `0004_block_positions.sql` | `5019860075ddc36d4aca97de660968c92b77f42efaabe70fe226b74f978696c7` | Adds, safely backfills, validates, and constrains native block positions on rows that persist complete block references. |

Each file contains its own `BEGIN`/`COMMIT`. None is idempotent and none records
an applied version or checksum in the database.

`0004_block_positions.sql` is finalized but has not been applied to any retained
database. It adds only eight initially nullable `bigint` columns on
`checkpoint`, `history`, and `journal`, accepts an explicit session allowlist of
exact Bitcoin/Ethereum scopes for a validated dense `position = height`
backfill, validates seven final coordinate constraints, and then makes the
three current-position columns non-null. Its checksum is locked above and by
the migration contract. It is deliberately absent from the current height-only
repository harness's default migration list because that writer cannot satisfy
the final columns. Applying `0004` and admitting the position-aware binary are
one fenced roll-forward transition whose runbook and runtime cutover remain
separate approval boundaries.

The external sequence and failure policy are specified in
[PostgreSQL Block-Coordinate Transition Runbook](POSTGRESQL_COORDINATE_TRANSITION_RUNBOOK.md).
The runbook is an operational contract, not authorization to access or migrate
a retained database.

## Effective final schema

All names below are unqualified in the scripts. They therefore resolve in the
session's active schema.

### Indexing-owned runtime state

| Relation | Final columns and constraints | Final supporting indexes |
|---|---|---|
| `checkpoint` | Existing scope, height, hash, parent, and timestamp fields; `position bigint NOT NULL`; nullable `parent_position bigint`; current position is non-negative and parent position/hash are both absent only at genesis, otherwise both present with a lower non-negative parent position; primary key `(chain, network)` | Primary-key index only |
| `history` | Existing canonical transaction, inclusion, status, fee, and primary-key fields; `block_position bigint NOT NULL`; nullable `block_parent_position bigint`; current position is non-negative and block parent position/hash obey the same atomic genesis rule | `history_by_height (chain, network, height)` plus the primary-key index |
| `movement` | Scope, address, height, transaction, ordinal, kind, movement identity, asset identity, and `amount numeric` are non-null; endpoints are nullable; `kind` is limited to `transfer`, `input`, `output`, `mint`, or `burn`; primary key `(chain, network, address, height, transaction_id, ordinal)`; no final foreign key to `history` | `movement_by_height (chain, network, height)` plus the primary-key index |
| `output` | Scope, output identity, address, asset identity, `amount numeric`, `evidence bytea`, `created_at bigint`, and `coinbase boolean` are non-null; primary key `(chain, network, transaction_id, output_index)` | `output_by_address_identity (chain, network, address, transaction_id, output_index)` and `output_by_height (chain, network, created_at)` |
| `journal` | Existing scope/current/previous checkpoint fields; `block_position bigint NOT NULL`; nullable current parent position and nullable previous checkpoint/parent positions; current coordinates obey the atomic genesis rule, while a previous checkpoint is either wholly absent or has non-negative position/height, hash, and its own atomic parent pair; timestamp remains optional when present; primary key `(chain, network, height)` | Primary-key index only |
| `journal_output` | Scope, journal height, output identity, address, asset identity, amount, evidence, creation height, and coinbase are non-null; primary key `(chain, network, height, transaction_id, output_index)`; foreign key `(chain, network, height)` references `journal` with `ON DELETE CASCADE` | Primary-key index; the referenced `journal` key is indexed by its primary key |

The indexing tables store generic `chain`, `network`, `asset_chain`, `asset`,
exact `numeric` amounts, and opaque byte evidence. Native SOL therefore requires
no Solana-only table and no asset-specific schema. Finalized migration `0004`
adds generic coordinate columns only to `checkpoint`, `history`, and `journal`;
the pending runtime cutover must consume that shape without rewriting any
canonical migration.

### Reusable SDK registry and custody state

`payment_wallets` has:

- primary key `id text`;
- non-null `chain`, `network`, `address`, `start_height`, `secret`, and
  `created_at` columns;
- `created_at timestamptz NOT NULL DEFAULT now()`;
- unique constraint `(chain, network, address)`; and
- index `payment_wallets_by_scope (chain, network)`.

This table belongs to the existing reusable SDK registry/restoration and
custody path. Its physical creation in `0001_init.sql` does not make it
synchronizer checkpoint/history/output state. A coordinate migration or
scope-local rescan must leave it byte-for-byte unchanged unless a separate
SDK-level custody change is approved. Initial native SOL support adds no row to
this table.

### Deployment-owned objects

The ordered migration history, target database, target schema, application of
the files, and applied-version/checksum evidence are deployment-owned. The
runtime adapter owns neither schema creation nor DDL. The database and schema
are shared infrastructure rather than an indexing object.

## Static adapter compatibility

Every referenced table, column, cast, constraint name, and conflict target in
the current adapter exists after all three migrations:

- checkpoint and retained-block reads map directly to `checkpoint` and
  `journal`, and their aliases satisfy the shared height-only `BlockRef` decoder;
- batched history, movement, output, journal, and checkpoint writes bind columns
  with matching PostgreSQL types, using canonical decimal text cast to
  `numeric`;
- history and movement pages use their primary-key order, while output pages use
  the final `output_by_address_identity` index from migration 0002;
- reorg reversal deletes movements explicitly using the index added by migration
  0003, then deletes history and created outputs, restores spent outputs, and
  removes the journal whose foreign key cascades to `journal_output`; and
- the current registry query and decoder match `payment_wallets`, including its
  uniqueness and scope-ordering index.

The static result is therefore **compatible for the current height-only source
shape**. The `Registry`, `RegisteredAddress`, and `payment_wallets` queries are
part of the existing reusable SDK persistence/restoration path and remain
intentionally preserved. The remaining explicit gaps are:

1. All migration and adapter SQL is unqualified. The current `pool(url,
   max_size)` accepts no schema and does not override a URL-supplied
   `search_path`. Static compatibility holds only when the intended schema is
   already first on the connection's path. This must fail closed before central
   runtime composition.
2. The schema permits negative heights, timestamps, output indexes, and wallet
   birthdays; half-populated fee or previous-checkpoint facts; and movement
   endpoint combinations that row decoders reject. Adapter-generated rows obey
   the domain rules, but read-only startup validation must reject incompatible
   retained rows before runtime use.
3. The adapter-safety sequence is closed by one-pool, one-schema proof for
   distinct scopes and native/token facts. The individual adapter/test defects
   were not evidence that the shared schema needs chain-specific tables.
   Zero-sized pools are rejected before URL parsing or pool construction, and
   spend deletion now matches the complete
   address-qualified `OutputKey`. Chain-neutral block validation also rejects
   one `OutputId` created under multiple addresses before repository SQL runs.
   Add/remove transactions now take a transaction-scoped advisory lock from
   the length-framed exact scope before reading even an absent checkpoint; no
   lock table or schema change is required. History pagination now reads its
   checkpoint, rows, movements, and verification checkpoint in one read-only
   repeatable-read snapshot. Output pagination applies the same snapshot rule
   to its checkpoint and live projection.
   Benchmark reset now deletes only its unique exact scope in dependency order
   and preserves unrelated scopes plus `payment_wallets`.

The former optional-test gap is closed: the owned PostgreSQL 18.6 harness runs
22/22 contracts without `POSTGRES_TEST_URL` or a skip path. It verifies the
three reviewed checksums, ordered effective catalog, ownership, constraints,
scope/index keys, and exact `payment_wallets` sentinel preservation.
Its shared-pool contract additionally rejects cross-scope reads/writes and
compares every unrelated indexing row before and after a different scope moves.

## Fresh and retained database paths

### Fresh named schema

A fresh, empty, explicitly selected schema for the future position-aware binary
receives `0001`, `0002`, `0003`, then `0004` exactly once. The PostgreSQL 18.6
fresh-schema contract proves `0004` succeeds with no indexing rows, validates
all seven coordinate checks, makes only current positions non-null, rejects
invalid writes, and preserves the registry sentinel. The current height-only
writer must continue to use the `0001`-through-`0003` baseline until its fenced
cutover. Deployment records the database identity, validated schema, PostgreSQL
server major, filename, ordinal, SHA-256, start/completion time, and result for
each file. Failure of any file stops the release; the runtime does not repair or
continue a partial history.

### Retained named schema

A retained schema never replays `0001`. Deployment first proves its known
baseline and applied checksums, inventories all scopes and application-owned
tables, and restores a copy from a tested restore point. It then applies only
the missing ordered migrations:

- a schema at the exact `0001` baseline receives `0002`, `0003`, then `0004`;
- a schema at the exact `0002` baseline receives `0003`, then `0004`;
- a schema at the exact `0003` height-only baseline receives only `0004`; and
- an exact finalized coordinate baseline receives nothing.

An unknown checksum, an incompatible definition under a required object name, a
partially applied migration, or a schema whose required shape cannot be tied to
the ordered history is a stop condition. Unrelated application-owned objects
are preserved, not treated as errors. The required objects must not be guessed
into compliance. Finalized migration `0004` is the additive transactional
native-position change: verified dense Bitcoin/Ethereum scopes may receive its
explicit scoped `position = height` backfill, while Solana, unknown chains, and
unverified custom scopes must never receive that inference.

That transition rule now has executable disposable evidence. The migration
session supplies a JSON array of exact `(chain, network)` pairs through
`payment_sdk.verified_dense_scopes`; only Bitcoin and Ethereum entries are
eligible. Before any update, the migration inventories populated scopes across
`checkpoint`, `history`, `movement`, `output`, `journal`, and `journal_output`
and aborts if one is not allowlisted. Its PostgreSQL 18.6 rehearsal preserved
the row count and SHA-256 signature of every baseline table after excluding
only the newly added coordinate keys, preserved the exact registry sentinel,
and produced complete current/parent coordinate pairs. Seven constraints are
then validated before current positions become non-null. Fresh invalid writes
prove null current positions, half-present parents, and incomplete previous
checkpoints are rejected. Populated Solana and invalid retained-parent fixtures
both prove errors roll back the entire transaction, including all eight column
additions. This is not evidence of a retained deployment; the external writer-
barrier runbook and position-aware runtime cutover remain pending.

## Deployment boundary and schema identity

The executor of record is external deployment automation using the PostgreSQL
18 `psql` client. `apps/api`, `sdk/indexing`, and
`sdk/indexing/postgres` never invoke it and never issue implicit DDL. This
checkout contains the ordered inputs but currently contains no deployment job
or applied-migration ledger; those operational artifacts must exist before a
retained-schema transition can be claimed complete.

The executor must:

1. validate the schema as lowercase ASCII `[a-z][a-z0-9_]{0,62}` and reject a
   `pg_` prefix;
2. connect to the explicitly named database with `ON_ERROR_STOP` behavior and
   pin the session path to exactly the validated schema followed by
   `pg_catalog`, before any file is read;
3. verify the resolved current schema and server major are the intended values;
4. apply one file at a time in canonical order without modifying its bytes; and
5. persist the exact evidence tuple listed above outside ordinary application
   logs, with credentials and secret data excluded.

Runtime pool construction must independently override any URL-supplied path and
pin every connection to exactly `<validated_schema>, pg_catalog`. Startup then
calls `indexing_postgres::validate_schema(&pool, configured_schema)`. The
validator uses one read-only repeatable-read transaction to confirm the
resolved schema identity and final required relations, columns, types,
nullability, constraints, indexes, and journal cascade. It reports a missing,
partial, wrong-schema, or decoder-incompatible schema before repositories are
constructed and never applies migrations or DDL. Owned PostgreSQL 18.6 tests
prove compatible, missing-relation, wrong-column, and wrong-schema outcomes
while preserving the exact registry sentinel.

## Preservation and restore proof

Before any retained-schema migration, deployment evidence must capture:

- database and schema identity, PostgreSQL version, migration evidence, and all
  relation definitions;
- per-`(chain, network)` row counts for every indexing-owned table;
- deterministic SHA-256 hashes of primary-key-ordered, type-stable exports of
  every indexing-owned row for each scope;
- total and per-scope `payment_wallets` counts plus non-secret metadata
  sentinels; and
- a restricted server-side SHA-256 comparison of each sentinel's `secret`
  bytes, never the bytes themselves and never in ordinary logs.

The same evidence is collected after migration and must match for every fact
the migration is not explicitly approved to change. A disposable integration
test additionally inserts known non-secret sentinel bytes and proves the entire
`payment_wallets` row is unchanged. Any mismatch aborts cutover.

A backup alone is insufficient. Deployment must restore the selected restore
point into an isolated PostgreSQL 18 instance, verify database/schema identity,
recompute the counts and hashes, run the read-only compatibility validator, and
prove the expected Bitcoin/Ethereum repository reads before the production
writer barrier or final migration is approved. Restore evidence records the
backup identifier, restore target, start/end time, verification results, and
cleanup; it contains no raw wallet secret.

## Owned PostgreSQL 18 test requirement

The required test environment pins an exact PostgreSQL 18 server image/artifact
and matching client by version and immutable digest. The harness owns a unique
disposable database or schema, ports, credentials, lifecycle, and cleanup. It
must fail—not skip—when PostgreSQL cannot start or when migrations cannot be
applied.

For every run, the harness must:

1. start the isolated server and fresh schema;
2. apply the three canonical files through the deployment boundary and verify
   their recorded SHA-256 values;
3. configure one process-wide pool whose connections are pinned to that schema;
4. run all PostgreSQL repository contracts with no `POSTGRES_TEST_URL` early
   return;
5. run adapter-safety, two-scope/shared-pool, native/token coexistence,
   migration-order, startup-validator, and retained-upgrade cases;
6. preserve a complete `payment_wallets` sentinel while exercising indexing
   add, pagination, rollback, scope-local cleanup, and restart; and
7. destroy only the owned disposable resources.

No runtime database, public network, funded key, or retained schema is part of
this proof. Owned non-skipping repository execution and baseline migration-
catalog/preservation evidence are now **run** against the pinned PostgreSQL
18.6 digest. Read-only startup compatibility validation is also **run** for
compatible, missing, wrong-column, and wrong-schema fixtures. Adapter-safety,
retained-upgrade, and multi-scope coexistence evidence remain **not run**.
