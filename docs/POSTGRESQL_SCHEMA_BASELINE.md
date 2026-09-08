# PostgreSQL Schema Baseline

## Status and evidence boundary

Payment-SDK is predeployment. On 2026-09-01 the unpublished PostgreSQL schema
history was consolidated into one fresh-schema initializer because no retained
production, staging, or shared development database requires an upgrade from an
older repository schema.

No retained database was inspected or changed during this reset. Any local
database created from the removed pre-release scripts must be discarded and
recreated before using the current binary. There is deliberately no supported
upgrade or backfill path from those unpublished intermediate schemas.

## Canonical artifact

The complete current schema is created by one file:

| Order | File | SHA-256 | Effective change |
|---:|---|---|---|
| 1 | `0001_init.sql` | `4d45ff45eab2c718ab3eb554a818a11391fde4ca8806ff26be782d9f40676b7c` | Creates the final position-aware indexing schema and reusable `payment_wallets` table. |

The initializer contains its own `BEGIN`/`COMMIT`, is not idempotent, and must
run exactly once against an empty, explicitly selected schema. It directly
creates the final catalog: there is no create-then-drop index, temporary
movement foreign key, coordinate `ALTER TABLE`, retained-data allowlist, or
height-to-position backfill.

The initializer becomes immutable when the first persistent database is
created. Every schema change after that boundary must be a new ordered migration
with its own checksum and preservation evidence; `0001_init.sql` must never be
edited or replayed over retained state.

## Effective schema

All names are unqualified in the initializer and therefore resolve in the
session's active schema.

### Indexing-owned runtime state

| Relation | Final columns and constraints | Supporting indexes |
|---|---|---|
| `checkpoint` | Scope, produced height, hash, optional parent hash and timestamp; `position bigint NOT NULL`; nullable `parent_position bigint`; position is non-negative and the parent position/hash pair is absent only at genesis | Primary-key index on `(chain, network)` |
| `history` | Canonical address transaction, status, fee, produced height, and block facts; `block_position bigint NOT NULL`; nullable `block_parent_position bigint`; position and parent completeness constraints | Primary key plus `history_by_height (chain, network, height)` |
| `movement` | Scope, address, transaction, ordinal, movement identity, asset identity, exact `numeric` amount, and optional endpoints; no foreign key to `history` | Primary key plus `movement_by_height (chain, network, height)` |
| `output` | Live UTXO identity, address, asset, exact amount, evidence, creation height, and coinbase flag | Primary key, `output_by_address_identity (chain, network, address, transaction_id, output_index)`, and `output_by_height (chain, network, created_at)` |
| `journal` | Current and previous complete block references, including native position and produced height; current and previous coordinate completeness constraints | Primary-key index on `(chain, network, height)` |
| `journal_output` | Spent outputs retained for rollback | Primary key and foreign key to `journal` with `ON DELETE CASCADE` |

The schema stores generic `chain`, `network`, `asset_chain`, and `asset` values,
exact `numeric` amounts, and opaque evidence bytes. Native SOL requires no
Solana-only or asset-specific table.

### Reusable SDK registry and custody state

`payment_wallets` has:

- primary key `id text`;
- non-null `chain`, `network`, `address`, `start_height`, `secret`, and
  `created_at` columns;
- `created_at timestamptz NOT NULL DEFAULT now()`;
- unique constraint `(chain, network, address)`; and
- index `payment_wallets_by_scope (chain, network)`.

This table belongs to the reusable SDK registry/restoration and custody path.
Physical creation beside indexing tables does not make it synchronizer state.
Indexing operations and future indexing migrations must preserve it unless a
separate SDK-level custody change is approved.

## Fresh database path

A deployment of the current predeployment baseline must:

1. create or select an empty named schema;
2. validate the schema name as lowercase ASCII `[a-z][a-z0-9_]{0,62}` and
   reject a `pg_` prefix;
3. connect with PostgreSQL 18 `psql` and `ON_ERROR_STOP` behavior;
4. pin `search_path` to exactly the selected schema followed by `pg_catalog`;
5. verify the resolved database, schema, and PostgreSQL server major;
6. verify the initializer SHA-256 above and apply `0001_init.sql` once; and
7. record the database identity, schema, server version, filename, ordinal,
   checksum, start/completion time, and result outside ordinary application
   logs.

If any required table already exists, initialization stops. The initializer is
not a repair script and must not be made idempotent with `IF NOT EXISTS`, because
that would hide partial or incompatible schemas.

Any developer, test, staging, or other predeployment database whose data still
matters must be exported or backed up before it is discarded. Generated wallet
secrets must never be printed or copied into ordinary logs.

## Runtime boundary and schema identity

The executor of record is external deployment automation using PostgreSQL 18
`psql`. `apps/api`, `sdk/indexing`, and `sdk/indexing/postgres` never apply the
initializer and never issue implicit DDL.

Runtime pool construction pins every connection to exactly
`<validated_schema>, pg_catalog`. Startup then calls
`indexing_postgres::validate_schema(&pool, configured_schema)`. Validation uses
one read-only repeatable-read transaction to confirm the resolved schema,
relations, column order and types, nullability, constraint families, indexes,
and journal cascade before repositories are constructed.

The checkout still contains no deployment job or database-backed applied-
migration ledger. Those deployment-owned artifacts must exist before the first
persistent environment is claimed operational.

## Owned PostgreSQL 18 evidence

The PostgreSQL test harness owns a unique disposable container and schema from
the checksum-pinned PostgreSQL 18.6 image. It fails rather than skips when the
server cannot start or the initializer cannot be applied.

For every run, the harness:

1. verifies the exact initializer checksum;
2. applies the initializer once to an empty schema;
3. inserts and preserves a complete `payment_wallets` sentinel;
4. verifies the effective columns, constraints, indexes, ownership, and journal
   cascade;
5. proves invalid coordinate rows are rejected;
6. proves the initializer refuses to replay over an existing schema;
7. runs repository, schema-validation, shared-pool, and scope-isolation
   contracts; and
8. destroys only its owned disposable resources.

No runtime database, public network, funded key, or retained schema is part of
this evidence.

## Future evolution boundary

Once the first persistent schema has received this initializer:

- freeze its exact bytes and checksum;
- add changes only as new lexical-order migration files;
- record applied versions and checksums in deployment-owned evidence;
- preserve unrelated chain/network scopes and `payment_wallets`;
- require a tested restore point and writer barrier for retained-data changes;
  and
- keep application startup read-only with respect to schema management.
