# PostgreSQL Schema Baseline

## Status and evidence boundary

This is the read-only schema baseline recorded on 2026-08-28 for the accepted
**Indexing & Central Database** decision. It is derived from the three SQL files
under `sdk/indexing/postgres/migrations/`, the complete current PostgreSQL
adapter, its examples and repository-contract tests, ADR-0026, ADR-0003, and the
central-database plan.

No SQL was executed, no database was contacted, and no retained state was
inspected. Consequently, this document proves the static relationship between
the checked-in scripts and source. It does not prove that any deployed database
has these migrations applied or that the optional PostgreSQL tests pass.

## Ordered canonical artifacts

The canonical order is lexical filename order. The files are immutable baseline
inputs: a later change must be a new ordered migration, not an edit to one of
these files.

| Order | File | SHA-256 | Effective change |
|---:|---|---|---|
| 1 | `0001_init.sql` | `1ca86f471b6cbe58880fcf42f4e2c433e29a0b3dc405fc1a03e517aed6bc886c` | Creates the complete height-only indexing schema and application-owned `payment_wallets` table. |
| 2 | `0002_output_pagination.sql` | `0949bfa6a51ceb8393ba879a0643512c8c6d915aa532d288623acbf55d79e6fb` | Replaces the prefix-only output-address index with the address-and-output-identity pagination index. |
| 3 | `0003_movement_cascade.sql` | `a9de19a7ede932b73463d62f9702133aad8bcd87b350f524679965c78c27a81b` | Adds the movement height index and removes the movement-to-history foreign key so reorg reversal deletes movements explicitly. |

Each file contains its own `BEGIN`/`COMMIT`. None is idempotent and none records
an applied version or checksum in the database.

## Effective final schema

All names below are unqualified in the scripts. They therefore resolve in the
session's active schema.

### Indexing-owned runtime state

| Relation | Final columns and constraints | Final supporting indexes |
|---|---|---|
| `checkpoint` | `chain text NOT NULL`, `network text NOT NULL`, `height bigint NOT NULL`, `hash bytea NOT NULL`, nullable `parent_hash bytea`, nullable `block_timestamp bigint`; primary key `(chain, network)` | Primary-key index only |
| `history` | `chain`, `network`, `address`, `transaction_id`, `status`, and `block_hash` are non-null text/bytea values; `height bigint NOT NULL`; nullable failure, parent, timestamp, and fee fields; `status` is limited to `included` or `failed`; primary key `(chain, network, address, height, transaction_id)` | `history_by_height (chain, network, height)` plus the primary-key index |
| `movement` | Scope, address, height, transaction, ordinal, kind, movement identity, asset identity, and `amount numeric` are non-null; endpoints are nullable; `kind` is limited to `transfer`, `input`, `output`, `mint`, or `burn`; primary key `(chain, network, address, height, transaction_id, ordinal)`; no final foreign key to `history` | `movement_by_height (chain, network, height)` plus the primary-key index |
| `output` | Scope, output identity, address, asset identity, `amount numeric`, `evidence bytea`, `created_at bigint`, and `coinbase boolean` are non-null; primary key `(chain, network, transaction_id, output_index)` | `output_by_address_identity (chain, network, address, transaction_id, output_index)` and `output_by_height (chain, network, created_at)` |
| `journal` | Scope, `height`, and `block_hash` are non-null; current parent/time and every previous-checkpoint field are nullable; primary key `(chain, network, height)` | Primary-key index only |
| `journal_output` | Scope, journal height, output identity, address, asset identity, amount, evidence, creation height, and coinbase are non-null; primary key `(chain, network, height, transaction_id, output_index)`; foreign key `(chain, network, height)` references `journal` with `ON DELETE CASCADE` | Primary-key index; the referenced `journal` key is indexed by its primary key |

The indexing tables store generic `chain`, `network`, `asset_chain`, `asset`,
exact `numeric` amounts, and opaque byte evidence. Native SOL therefore requires
no Solana-only table and no asset-specific schema. The accepted native-position
cutover will later add generic coordinate columns only to `checkpoint`,
`history`, and `journal`; it must not rewrite these baseline migrations.

### Application-owned state

`payment_wallets` has:

- primary key `id text`;
- non-null `chain`, `network`, `address`, `start_height`, `secret`, and
  `created_at` columns;
- `created_at timestamptz NOT NULL DEFAULT now()`;
- unique constraint `(chain, network, address)`; and
- index `payment_wallets_by_scope (chain, network)`.

This table is application/custody state. Its physical creation in
`0001_init.sql` does not give the indexing domain ownership. An indexing
migration or scope-local rescan must leave it byte-for-byte unchanged unless a
separate application-owned change is approved. Initial native SOL support adds
no row to this table.

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
shape**, subject to the following explicit gaps:

1. `registry.rs` still reads and writes `payment_wallets`. That is compatible
   with the baseline SQL but violates the accepted target ownership. It remains
   temporarily necessary only so retained wallet rows are not stranded. The
   application-owned restart path must be implemented and proven first; then
   the indexing `Registry` surface and queries must be removed without dropping
   or changing the table.
2. All migration and adapter SQL is unqualified. The current `pool(url,
   max_size)` accepts no schema and does not override a URL-supplied
   `search_path`. Static compatibility holds only when the intended schema is
   already first on the connection's path. This must fail closed before central
   runtime composition.
3. The schema permits negative heights, timestamps, output indexes, and wallet
   birthdays; half-populated fee or previous-checkpoint facts; and movement
   endpoint combinations that row decoders reject. Adapter-generated rows obey
   the domain rules, but read-only startup validation must reject incompatible
   retained rows before runtime use.
4. The spend query matches output identity but not the `OutputKey` address; the
   empty-scope first-commit lock, checkpoint-bound page snapshot, zero-sized
   pool, and global benchmark reset also need the repairs already listed under
   **Adapter Safety**. These are adapter/test defects, not evidence that the
   shared schema needs chain-specific tables.
5. The eight repository-contract tests return early when `POSTGRES_TEST_URL` is
   absent, and their header mentions only migration 0001. A green default suite
   is not PostgreSQL execution evidence and the full three-file baseline is
   required.

## Fresh and retained database paths

### Fresh named schema

A fresh, empty, explicitly selected schema receives `0001`, `0002`, then `0003`
exactly once. Deployment records the database identity, validated schema,
PostgreSQL server major, filename, ordinal, SHA-256, start/completion time, and
result for each file. Failure of any file stops the release; the runtime does
not repair or continue a partial history.

### Retained named schema

A retained schema never replays `0001`. Deployment first proves its known
baseline and applied checksums, inventories all scopes and application-owned
tables, and restores a copy from a tested restore point. It then applies only
the missing ordered migrations:

- a schema at the exact `0001` baseline receives `0002`, then `0003`;
- a schema at the exact `0002` baseline receives only `0003`; and
- an exact final baseline receives nothing.

An unknown checksum, an incompatible definition under a required object name, a
partially applied migration, or a schema whose required shape cannot be tied to
the ordered history is a stop condition. Unrelated application-owned objects
are preserved, not treated as errors. The required objects must not be guessed
into compliance. Future native-position work is a new additive, transactional
migration: verified dense Bitcoin/Ethereum scopes may receive an explicit
scoped `position = height` backfill, while Solana, unknown chains, and
unverified custom scopes must never receive that inference.

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
uses a read-only catalog validator to confirm the final required relations,
columns, types, nullability, constraints, and indexes. It reports a missing,
partial, extra-conflicting, or decoder-incompatible schema and exits before
constructing repositories. It does not apply migrations.

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
this proof. Until this owned non-skipping suite runs, PostgreSQL execution and
preservation remain **not run**, even when the ordinary workspace test command
is green.
