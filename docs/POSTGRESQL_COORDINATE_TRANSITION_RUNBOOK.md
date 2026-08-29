# PostgreSQL Block-Coordinate Transition Runbook

## Scope and authorization boundary

This runbook governs the retained-schema transition from the height-only
`0001` through `0003` baseline to finalized migration
`0004_block_positions.sql`. It prepares an external deployment procedure; it
does not authorize applying the migration to any retained database.

Execution requires a separate change record naming the exact database, schema,
restore point, old-writer fence, position-aware release artifact, operator,
maintenance window, verification owner, and failure response. Application and
SDK processes never execute DDL.

The migration has SHA-256:

```text
5019860075ddc36d4aca97de660968c92b77f42efaabe70fe226b74f978696c7
```

Only explicitly verified Bitcoin and Ethereum `(chain, network)` scopes may be
listed for dense `position = height` backfill. A populated Solana, unknown, or
unverified scope is a stop condition. `payment_wallets` belongs to the reusable
SDK registry/custody path and must remain byte-for-byte unchanged.

## Required evidence package

Create one restricted deployment evidence package outside ordinary application
logs. It must contain:

- change-record identity, operator, reviewer, start/end time, and result;
- database host identity, database name, exact schema, PostgreSQL major, and
  server identity evidence;
- applied migration filenames and checksums through `0003`;
- the reviewed `0004` checksum above;
- the exact JSON allowlist of verified dense scopes;
- definitions, row counts, and deterministic content hashes for all six
  indexing tables, grouped by exact scope;
- total and per-scope `payment_wallets` counts, non-secret metadata hashes, and
  a restricted comparison hash of secret bytes without revealing those bytes;
- restore-point identity and a successful disposable restore proof;
- evidence that every old writer is stopped and fenced from restarting;
- post-migration column, constraint, row-count, content-hash, and registry
  preservation results; and
- the exact position-aware release identity admitted after verification.

Never include credentials, wallet secrets, seed material, or private keys in
the package.

## Preflight

### 1. Resolve exact targets

Use deployment-owned values; do not infer them from a developer workstation:

```text
PAYMENT_DATABASE_URL=<credential reference resolved by deployment tooling>
PAYMENT_SCHEMA=<lowercase explicit schema>
PAYMENT_DENSE_SCOPES=<compact JSON array of exact Bitcoin/Ethereum scopes>
PAYMENT_OLD_RELEASE=<currently running height-only release identity>
PAYMENT_NEW_RELEASE=<reviewed position-aware release identity>
PAYMENT_RESTORE_POINT=<immutable backup or snapshot identity>
```

The schema must match `[a-z][a-z0-9_]{0,62}`, must not start with `pg_`, and
must be quoted as an identifier by deployment tooling. Reject an empty or
default-derived target.

### 2. Verify server and session identity

Using PostgreSQL 18 `psql` with `ON_ERROR_STOP=1`, connect read-only and record:

```sql
SELECT current_database(), current_user, current_schema(), version();
SHOW search_path;
SELECT pg_is_in_recovery();
```

The resolved schema must be exactly the approved schema. Pin the session path
to `<approved_schema>, pg_catalog`; do not rely on a URL-provided or role-level
default. A server, database, schema, or PostgreSQL-major mismatch stops the
transition.

### 3. Prove the applied baseline

Verify that the retained schema matches the reviewed effective result of:

| Order | File | SHA-256 |
|---:|---|---|
| 1 | `0001_init.sql` | `1ca86f471b6cbe58880fcf42f4e2c433e29a0b3dc405fc1a03e517aed6bc886c` |
| 2 | `0002_output_pagination.sql` | `0949bfa6a51ceb8393ba879a0643512c8c6d915aa532d288623acbf55d79e6fb` |
| 3 | `0003_movement_cascade.sql` | `a9de19a7ede932b73463d62f9702133aad8bcd87b350f524679965c78c27a81b` |

An absent ledger, unknown checksum, partially applied file, unexpected object
definition, or already-partial coordinate expansion is a stop condition. Do
not repair or guess the schema into compliance during this change.

### 4. Inventory populated scopes

Read the union of exact scopes from `checkpoint`, `history`, `movement`,
`output`, `journal`, and `journal_output`. Every result must appear exactly in
the reviewed dense-scope allowlist. Do not include `payment_wallets` in the
backfill allowlist because it is not indexing state.

For every indexing table and scope, record a row count and a deterministic hash
of a primary-key-ordered, type-stable export. Record the registry preservation
evidence described above separately. The preflight export mechanism must be
reused unchanged after migration.

### 5. Prove restoration

Restore `PAYMENT_RESTORE_POINT` into an owned disposable PostgreSQL 18 target.
Verify database/schema identity, baseline checksums, object definitions, all
scope counts/hashes, and registry evidence against the source package. Apply
`0004` to this restored copy using the exact allowlist and application sequence
below, then run every post-migration check. Destroy only this owned disposable
copy after preserving its non-secret evidence.

A restore that was not opened and compared is not a tested restore point.

## Writer barrier

### 6. Close admission and drain old work

Close public and internal admission for every operation that can cause an
indexing commit. Allow already admitted synchronization commits to finish.
Record the final old-writer checkpoint for every exact scope.

### 7. Stop every height-only writer

Stop all instances, jobs, workers, maintenance commands, and standby processes
that can write the six indexing tables. Read-only traffic may remain only if it
cannot trigger writes or automatic restart.

### 8. Fence restart and replacement

Apply a deployment-level fence that prevents `PAYMENT_OLD_RELEASE` and any
unreviewed artifact from starting. Scaling to zero without disabling automatic
restart is not a fence. Record evidence from the scheduler/process supervisor
that zero old writers remain and cannot be recreated during the transition.

Re-read database activity and scope checkpoints after the fence. If any scope
moves, the barrier failed: stop, repair the fence, discard the attempted
evidence window, and restart preflight from a new restore point.

## Apply finalized migration 0004

### 9. Verify the file immediately before use

From the reviewed checkout, compute SHA-256 for
`sdk/indexing/postgres/migrations/0004_block_positions.sql` and require the
exact recorded value. Do not edit, copy-paste, normalize, or wrap the migration
bytes.

### 10. Open one pinned migration session

Use one PostgreSQL 18 `psql` session with:

- `ON_ERROR_STOP=1`;
- the exact approved database;
- search path pinned to `<approved_schema>, pg_catalog`; and
- output redirected to the restricted evidence package with secrets excluded.

Inside that same session, first set the compact reviewed JSON allowlist:

```sql
SELECT set_config(
    'payment_sdk.verified_dense_scopes',
    '<PAYMENT_DENSE_SCOPES>',
    false
);
```

Then use `psql`'s `\i` command to execute the reviewed migration file directly.
The setting and migration must share one session. The migration owns its
`BEGIN`/`COMMIT`; deployment tooling must not nest it in another transaction.

The migration atomically:

1. adds all eight initially nullable position columns;
2. rejects an ineligible allowlist or any populated unverified scope;
3. validates dense parent relationships;
4. backfills only allowlisted Bitcoin/Ethereum scopes;
5. validates the complete backfill;
6. adds and validates seven final coordinate constraints; and
7. makes checkpoint, history, and journal current positions non-null.

Any error is a failed transition. Issue `ROLLBACK` only to close a session left
in an aborted transaction, preserve the error evidence, and do not admit any
writer.

## Post-migration verification

### 11. Verify catalog shape

Require exactly these new columns:

- `checkpoint.position bigint NOT NULL` and nullable `parent_position`;
- `history.block_position bigint NOT NULL` and nullable
  `block_parent_position`; and
- `journal.block_position bigint NOT NULL`, nullable
  `block_parent_position`, nullable `previous_checkpoint_position`, and nullable
  `previous_checkpoint_parent_position`.

Require these constraints to exist and be validated:

```text
checkpoint_position_nonnegative
checkpoint_parent_complete
history_block_position_nonnegative
history_block_parent_complete
journal_block_position_nonnegative
journal_block_parent_complete
journal_previous_checkpoint_complete
```

No coordinate column may exist on `movement`, `output`, `journal_output`, or
`payment_wallets`.

### 12. Verify data and preservation

For every allowlisted dense scope, require:

- current position equals produced height;
- parent position is absent exactly when parent hash is absent;
- a present parent position equals produced height minus one;
- previous checkpoint position equals previous checkpoint height; and
- a present previous parent position equals previous checkpoint height minus
  one.

Repeat the exact preflight count/hash exports. All pre-existing columns in all
six indexing tables must match their preflight counts and hashes. Every
registry count, metadata hash, restricted secret-byte hash, and sentinel must
match byte-for-byte. Any mismatch is a failed transition.

### 13. Run read-only startup validation

Run the reviewed position-aware release's schema validator against the exact
schema before constructing repositories or opening admission. A validator from
the old height-only release is not release evidence.

## Admit only the position-aware binary

### 14. Preserve the old-writer fence

Keep `PAYMENT_OLD_RELEASE` fenced permanently. Migration `0004` makes its
height-only inserts invalid; restarting it is prohibited even as a rollback.

### 15. Start the reviewed release without admission

Start only `PAYMENT_NEW_RELEASE`. Require successful schema identity and
coordinate validation, registry restoration through the existing reusable SDK
path, and position-aware repository construction before synchronization.

### 16. Verify scope continuity and readiness

For every scope, compare the first position-aware checkpoint with the recorded
final old-writer checkpoint. Run catch-up with admission closed. Require the
persisted checkpoint and readiness contract to pass before opening the public
listener or transaction admission.

### 17. Close the change record

Repeat scope counts, constraint validation, registry preservation checks, and
release identity checks. Store the complete non-secret evidence package and
mark the applied `0004` filename/checksum/result in the deployment-owned
migration ledger.

## Failure and recovery policy

### Failure before migration commit

Keep all writers fenced. Verify the schema remains at the exact `0001` through
`0003` baseline and that counts/hashes match preflight. Only then may the exact
old release be unfenced. If any DDL or data differs, restore the tested restore
point instead of repairing rows manually.

### Failure after migration commit but before new-writer admission

This is roll-forward only. Keep the old release fenced, preserve the database,
correct the position-aware release or verification procedure, and retry from
the post-migration validation step. Do not drop constraints, null positions, or
restart the height-only writer as an operational shortcut.

### Failure after new-writer admission

Close admission and fence the position-aware writer. Preserve all evidence and
scope checkpoints. Recovery uses a reviewed position-aware release. Restoring
the pre-migration snapshot discards post-cutover commits and therefore requires
a separate destructive-recovery authorization and reconciliation plan.

### Unknown or mismatched evidence

Stop. Do not continue when schema identity, migration checksum, scope
allowlist, restore comparison, writer fence, row hash, constraint validation,
registry preservation, or release identity is unknown or mismatched.
