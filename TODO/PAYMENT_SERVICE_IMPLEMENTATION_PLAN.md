# Ethereum v1 Payment Service and Wallet HTTP Runtime

Status: The core Ethereum v1 implementation is present in source as of
2026-08-05. This plan remains open for the acceptance work listed below; source
and deterministic tests are not live deployment evidence.

Implemented source scope:

- Authenticated PS `/v1` APIs, durable command idempotency and UUIDv7 resource
  IDs, users, jobs, deposits, explicit accounting, and typed reconciliation.
- IX-owned deposit birthdays, recoverable `AwaitingWatch`, append-only IX
  mirroring, storage-aware classification, checked absolute ledger projection,
  and a durable per-deposit observation index.
- A required versioned policy with one Ethereum scope, numeric `chain_id`,
  explicit asset destinations/thresholds, gas-funder limits, and fee ceilings.
- Native ETH and ERC-20 collection aggregates, reservations, durable gas-fund
  and sweep legs, exact-envelope replay, IX watches, explicit retry, and reorg
  correction.
- Stateless authenticated Ethereum WS HTTP composition using concrete RPC and
  the vendor-neutral remote-custody client; neither process stores custody
  secrets in PS state.
- Separate PS RocksDB ownership, fail-closed metadata validation, verified
  physical backup, and explicit semantic schema-v2 migration/index rebuild.

Remaining acceptance work:

- Make IX-driven collection-leg transitions and their corresponding
  ledger/reconciliation/projection-cursor changes one physical PS storage
  transaction. Current handling is replay-safe and idempotent but crosses two
  durable commits.
- Complete the failure-window, restart, full collection-workflow, runtime
  supervision, and readiness recovery test matrix described below.
- Run and record the opt-in disposable Anvil PS/WS/IX scenario and real-node
  operational checks; integrate and assess the external durable custody
  service separately.
- HA and one PS database owning multiple network scopes remain excluded from
  v1, not unfinished claims of this implementation.

## Summary

Implement a production-oriented, single-network Ethereum Payment Service in
`apps/api`, backed by one exclusive RocksDB database and an isolated stateless
Wallet Service in `apps/wallet`.

Locked scope:

- Internal exchange-backend JSON/HTTP API.
- Native ETH and explicitly allowlisted standard ERC-20 assets.
- Durable jobs for deposit creation, closure, collection, and retries.
- Explicit accounting and collection commands; no automatic financial
  decisions.
- Separate ordinary and administrator bearer credentials.
- Polling only; no webhooks or message broker.
- Separate authenticated WS HTTP process.
- Vendor-neutral remote custody API; tests use fake/ephemeral custody.
- Existing PS RocksDB records preserved through an explicit migration.
- Existing Ethereum IX v1 lifecycle retained: `Included`, `Failed`,
  depth-based `Confirmed`, and `Reorged`.

## Public and Internal Interfaces

### Payment Service API

Use `/v1`, JSON, mandatory TLS termination for non-loopback listeners, bounded
bodies, and opaque cursor pagination. Encode atomic amounts and large integers
as unsigned decimal strings.

Ordinary exchange token:

- `POST /v1/deposits` with `Idempotency-Key` and
  `{user_id, scope, asset, expected_amount}`. Always return `202` with stable
  `job_id` and `deposit_id`; never expose the address before IX watch
  acknowledgement.
- `GET /v1/jobs/{job_id}`.
- `GET /v1/deposits` with optional `user_id`, state, and cursor filters.
- `GET /v1/deposits/{deposit_id}` for lifecycle, payment progress, address when
  active, expected amount, and expiration.
- `GET /v1/deposits/{deposit_id}/balances`.
- `GET /v1/deposits/{deposit_id}/ledger`.
- `GET /v1/deposits/{deposit_id}/observations`.
- `POST /v1/deposits/{deposit_id}/close`, returning a job.
- `POST /v1/collections` with `{deposit_id}`, deriving destination and amount
  from policy and spendable balance.
- `GET /v1/collections` and `GET /v1/collections/{collection_id}`.
- `POST /v1/collections/{collection_id}/retry`, returning a durable retry job.

Administrator token:

- `POST /v1/deposits/{deposit_id}/accounting` with absolute `next_accounted`,
  expected ledger head, reason, and idempotency key.
- `GET /v1/reconciliations` and `GET /v1/reconciliations/{case_id}`.
- `POST /v1/reconciliations/{case_id}/resolve` using `reverse_credit`,
  `accept_liability`, or `external_debt_recorded`.
- `GET /v1/admin/status` for dependency health, cursors, lag, job backlog, and
  active policy version.

The admin token may access ordinary routes; the ordinary token must receive
`403` on admin routes. Every external mutation uses idempotency scoped by
authenticated principal, operation, and request hash; reuse with different
content returns `409`.

Use the stable error envelope:

```json
{
  "code": "machine_readable_code",
  "message": "safe contextual message",
  "retryable": false,
  "request_id": "ps-request-..."
}
```

Jobs use `queued`, `running`, `waiting_retry`, `succeeded`, or `failed`, with
resource ID, attempt count, safe last error, and optional next-attempt time.
`job_id` and other server-owned PS resource IDs use lowercase-prefixed UUIDv7
values. PS generates them once and atomically stores them with the scoped
command-idempotency record. Replaying the same command returns the same IDs;
reusing the key with different content returns `409`.

Deposit payment progress is derived without changing accounting:

- `unseen`: no canonical received amount.
- `included`: value observed but none confirmed.
- `partial`: confirmed value is below expected.
- `paid`: confirmed equals expected.
- `overpaid`: confirmed exceeds expected.

Expiration is independent: late, partial, and excess funds remain indexed and
visible, but never trigger automatic credit or collection.

### PS-to-WS API

Expose authenticated, Ethereum-scoped internal operations:

- Address generation.
- Balance query.
- Sign native/ERC-20 transfer.
- Collection prerequisite calculation.
- Sign native/ERC-20 collection.
- Broadcast an exact signed envelope.
- Read transaction receipt.

Signing responses contain canonical transaction ID, opaque signed envelope,
and collection attribution. They are internal only and must never appear in
public responses or logs.

PS persists the signed envelope before broadcast, retries the exact bytes after
response loss, verifies the expected hash, and treats an RPC `already known`
response as success only when the hash matches.

### Remote custody contract

Add a chain-independent remote signer/provisioner adapter with authenticated
endpoints for:

- Idempotent key provisioning by operation ID, curve, public-key format, and
  purpose.
- Public-key lookup by opaque locator.
- Idempotent message/digest signing by operation ID, locator, scheme, encoding,
  and interaction policy.
- Readiness/capability reporting.

Reusing an operation ID with different content returns a conflict. No endpoint
exports secret material. The external custody backend itself remains outside
this repository.

## Implementation Changes

### Domain and persistence hardening

- Persist a minimal PS-owned `User` record containing only an opaque `UserId`
  and the authenticated exchange principal that owns it. The exchange may
  supply the opaque identifier; PS owns its durable record and deposit
  associations, not customer PII or identity profiles.
- Atomically persist every deposit's opaque `KeyLocator`, key purpose, and
  ownership metadata with the deposit. PS never stores raw private keys, seed
  material, or signer credentials.
- Add checked add/subtract, zero checks, and decimal parsing/formatting to
  `AtomicAmount`; reject overflow and underflow.
- Bind every PS database to immutable metadata: service owner, schema version,
  one Ethereum `IndexScope`, and active policy history. Fail startup if pointed
  at IX storage or a different network.
- Extend deposit address requests with asset and explicit key-purpose data.
  Capture birthday from the IX Ready checkpoint, not WS, and correct the stale
  documentation diagram.
- Add durable command-idempotency lookup before generating server IDs or
  provisioning keys.
- Add user-to-deposit, deposit-to-observation, transaction-to-collection-leg,
  job, collection, reservation, and unresolved-projection indexes.
- Preserve chain neutrality: Ethereum parsing and envelope validation remain in
  the Ethereum crate; PS stores only canonical identifiers and opaque signed
  bytes.
- Preserve retryability through domain/application errors instead of collapsing
  source errors to strings.
- Replace caller-calculated ledger snapshots with a pure, checked transition
  engine owned by the deposit domain.
- Make classification storage-aware through an async context resolver plus a
  pure classifier. Add explicit `OtherBalanceChange`.
- Persist unresolved classifications and stop the projection cursor/readiness
  until resolved; never silently advance past them.
- Keep the PS copy of relevant IX events immutable and append-only. Deduplicate
  delivery by immutable event ID plus observation revision, never by
  `(transaction_id, status)`, and use the IX cursor only for ordering.
- Access IX exclusively through its semantic status/watch/query/replay HTTP
  API. PS never opens or writes the IX database.

Collection persistence must include:

- Idempotency identity, aggregate state, policy version, timestamps, attempts,
  and safe error state.
- One active reservation per deposit/asset.
- Ordered `GasFunding` and `Sweep` legs.
- Leg states `Required`, `Signed`, `Broadcast`, `Confirmed`, `Failed`, and
  `Reorged`.
- Canonical transaction ID, optional IX watch ID, attribution, gross debit,
  master credit, and allocated fee.
- A redacted signed-envelope record retained through unknown broadcast outcome
  and deleted atomically after accepted broadcast. Its expiry is an operational
  alert/retention hint, never permission to sign replacement bytes. Production
  deployment requires encrypted storage volumes.

### Deposit, projection, and accounting workflows

- Deposit jobs wait for IX Ready, capture its checkpoint, request an idempotent
  WS address, atomically persist `AwaitingWatch` plus the zero ledger row,
  register the IX watch, activate the deposit, then complete the job.
- Transient IX/WS/custody errors move jobs to `waiting_retry`; terminal
  validation or policy errors fail them.
- Expiration marks the deposit `Expired` but retains its IX watch so late
  payments remain observable.
- Closing is allowed only with an exact zero-balance ledger head, no active
  reservation/collection, and no open reconciliation. The close commit
  conditions the ledger and business generations atomically. Ethereum v1
  retains the IX watch after `Closed`; reclaiming it requires a future durable
  IX cutoff-and-PS-drain protocol so in-flight payments cannot disappear.
- Continuously mirror IX events, then project them independently in cursor
  order.
- Classifier precedence is collection transaction mapping, gas-funding mapping,
  incoming movement to a known deposit, then unexplained outgoing/other balance
  change.
- Group all movements affecting one deposit and append at most one absolute
  ledger row per deposit/event.
- `Included` changes canonical received/balance, `Confirmed` changes
  confirmation-qualified fields, and `Reorged` applies checked inverse
  corrections. Observation projection never changes `accounted`.
- Accounting is an immediate administrator command using absolute
  `next_accounted`, expected-head concurrency, and independent idempotency.
- Post-credit reorgs preserve `accounted`, append corrected canonical balances,
  and open a blocking case. Resolution is atomic:
  - `reverse_credit`: append the accounting correction and resolve.
  - `accept_liability`: preserve accounted and record the accepted gap.
  - `external_debt_recorded`: preserve accounted and require an external
    reference.

### Ethereum collection workflow

Load a mandatory versioned JSON policy containing:

- Immutable scope and deposit TTL.
- Asset allowlist.
- Per-asset master destination and minimum collection amount.
- Fee/gas ceilings.
- Dedicated gas-funder address and opaque key locator.
- Maximum gas-funding amount.

Reject duplicate assets, scope mismatches, invalid addresses, missing limits,
and nonstandard token policies. Record the policy version/digest on every job
and collection.

Collection execution:

1. Validate deposit eligibility and absence of reconciliation blocks.
2. Atomically reserve the full spendable eligible amount.
3. Query WS balance and collection requirements.
4. For native ETH, sign and persist one sweep transaction.
5. For ERC-20, create a durable gas-funding leg when required,
   sign/persist/broadcast/watch it, and wait for confirmation before signing the
   token sweep.
6. Broadcast persisted bytes, record acceptance, then idempotently register
   `watch(txid)`.
7. Update leg and ledger state only from IX facts.
8. Update `collected` only on confirmed sweep facts.
9. On transient dependency failures, retry automatically with bounded backoff.
10. On terminal failure, reorg, or future dropped/replaced facts, release or
    correct reservations and require the explicit retry endpoint before signing
    a new attempt.

Use one collection executor for v1 and a dedicated gas-funder key to avoid
concurrent master-nonce conflicts. Do not implement replacement or fee-bumping
in this release.

### Runtime, WS, and operations

- Add `serve`, `backup`, and explicit `migrate` commands while retaining current
  bounded maintenance commands.
- Open RocksDB and clients once; supervise HTTP, metrics, watch reconciliation,
  IX ingestion, projection, expiration, collection, and readiness tasks.
- Reuse the existing HTTP security/health primitives and Indexer supervision
  pattern, adding both SIGINT and SIGTERM, readiness false-before-drain, bounded
  shutdown, and fatal-task propagation.
- Readiness requires valid DB metadata/policy, healthy WS/IX dependencies within
  thresholds, and bounded ingestion/projection lag. Liveness remains
  detail-free.
- Expose Prometheus only on loopback; never use user IDs, addresses, locators,
  transaction envelopes, or credentials as metric labels.
- Implement a concrete Ethereum HTTP RPC adapter in the Ethereum crate.
- Implement the Wallet HTTP server as a stateless process using the Ethereum
  adapter plus the remote signer/provisioner. It owns no deposits, jobs,
  workflow state, or database.
- All non-loopback PS, WS, and custody traffic requires trusted TLS termination
  and bearer authentication.

### Migration and documentation

- Require a verified backup before migration; `serve` must fail closed on an
  older schema.
- Bind existing data to an operator-supplied Ethereum network because old
  deposits lack persisted network identity.
- Preserve existing deposit, ledger, event mirror, cursor, and reconciliation
  records; add supplementary indexes and validate counts/references.
- Reject migration if records contain a non-Ethereum chain or broken
  journal/index references.
- Document restore/rollback using the verified backup.
- Update canonical requirements with the selected API/auth/job policies,
  IX-owned birthday, singleton topology, explicit business commands,
  signed-before-broadcast recovery, and the Ethereum v1 exclusions.

## Test and Acceptance Plan

- Unit-test 256-bit decimal conversion and checked arithmetic, policy
  validation, DTO parsing, authorization, error mapping, and redaction.
- Test pure classification/ledger transitions across
  Included -> Confirmed -> Reorged -> re-included, partial/overpayment,
  multiple movements, multiple deposits, unexpected outgoing movements, and
  unresolved classification.
- Real-RocksDB tests for command idempotency, user/history indexes, jobs,
  collection reservations, expected-state transitions, transaction indexes,
  structured reconciliation, and database owner/scope mismatch.
- Migration fixture proving current deposits, ledger chains, IX cursors,
  mirrored events, and reconciliation cases survive backup/migration/reopen.
- HTTP tests for both credentials, route/method failures, body/page bounds,
  decimal overflow, changed-payload idempotency conflicts, and address
  non-disclosure before activation.
- Failure-window tests for lost address responses, crash before/after
  `AwaitingWatch`, lost IX acknowledgement, lost signing response, crash after
  signed-envelope persistence, lost broadcast response, and crash before IX
  transaction-watch attachment.
- Workflow tests for native ETH collection, ERC-20 with and without gas
  prefunding, confirmation-only collection accounting, terminal failure,
  explicit retry, and collection reorg correction.
- Restart tests proving all jobs and workers resume without duplicating keys,
  ledger rows, broadcasts, watches, credits, or reservations.
- Runtime tests for readiness degradation/recovery, dependency timeouts, worker
  failure propagation, graceful SIGTERM/SIGINT, metrics, and
  secret/signed-envelope log redaction.
- Add an opt-in Anvil end-to-end test using disposable funds and a deterministic
  fake remote custody service; no production network calls in ordinary tests.
- Final validation: formatting, diff checks, locked full workspace check/tests,
  full Clippy with warnings denied, documentation build, and targeted
  PS/WS/Ethereum tests.

## Assumptions and Exclusions

- Ethereum v1 deployment constraint: each PS instance exclusively owns one PS
  RocksDB path and consumes one IX event feed for one Ethereum `IndexScope`.
  Multiple networks require separate PS instances/databases or a future
  scope-keyed persistence redesign.
- The trusted exchange backend supplies opaque `user_id`; PS owns associations,
  not customer identity profiles.
- Only native ETH and standard ERC-20 `Transfer` behavior are
  production-supported.
- No Bitcoin, imported/watch-only addresses, direct end-user authentication,
  webhooks, queues, automatic credit, automatic collection, Trezor, local
  production custody, HA, traces, nonstandard tokens, fee bumping, or
  replacement workflows.
- PS models dropped/replaced facts but does not claim live detection while
  Ethereum IX remains at v1.
- The remote custody service must provide durable idempotency and key retention;
  tests do not make the ephemeral local signer a production dependency.
- Policy numeric values have no insecure defaults: deployment must explicitly
  provide TTL, thresholds, fee ceilings, master destinations, and gas-funding
  limits.
