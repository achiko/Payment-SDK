# Payment Service Postman manual-testing guide

This guide is for a manual tester using
[PAYMENT_SERVICE.postman_collection.json](./PAYMENT_SERVICE.postman_collection.json)
against the single-network Ethereum Payment Service (PS).

The collection covers all 17 authenticated PS operations, public health
endpoints, loopback metrics, three supported reconciliation bodies, and safe
authentication/routing checks. It intentionally contains no bearer-token
values. Its role-specific request layout assumes
`STRICT_AUTHENTICATION_MODE=true`; global-trusted deployments should use a
separate environment and omit bearer headers.

For local service startup, follow
[PAYMENT_SERVICE_USAGE.md](./PAYMENT_SERVICE_USAGE.md#step-by-step-run-the-service)
first.

## Safety rules

- Use a disposable local Ethereum chain, disposable IX and PS databases, and the
  loopback-only local custody process.
- Never point this collection at production until every mutation has a
  reviewed test plan and explicit authorization.
- Never use **Run collection**. The collection contains lifecycle-changing
  accounting, collection, close, and reconciliation requests.
- Never fund an ephemeral custody address with real assets.
- Do not put bearer tokens in the committed collection, screenshots, Postman
  console output, exported environments, or test reports.
- Do not execute Create Collection or Retry Collection merely to see what
  happens. Those workflows can sign and broadcast transactions.
- Keep local custody running for the whole test. Its keys disappear on exit.
  If it restarts, discard the disposable PS/IX databases and restart the local
  scenario so stored locators cannot refer to missing keys.

## 1. Prerequisites

The following processes must be running:

| Process | Default address |
|---|---|
| Ethereum JSON-RPC node | http://127.0.0.1:8545 |
| Indexer Service | http://127.0.0.1:8080 |
| Local ephemeral custody | http://127.0.0.1:8181 |
| Wallet Service | http://127.0.0.1:8082 |
| Payment Service API | http://127.0.0.1:8081 |
| Payment Service metrics | http://127.0.0.1:9091 |

PS, WS, IX, and the policy must all refer to the same disposable network. A
common one-command launcher setup uses:

~~~text
chain: ethereum
network: local
chain ID: 31337
~~~

If you instead follow the manual Anvil sequence in the usage guide, set the
Postman `network` variable to `anvil`. In every case, this value must exactly
match the IX scope and PS policy.

Verify that PS is ready before opening Postman:

~~~bash
curl --fail-with-body --silent --show-error http://127.0.0.1:8081/health/ready
~~~

Expected:

~~~json
{"status":"ready","authentication_mode":"strict"}
~~~

## 2. Import the collection

1. Open Postman.
2. Select **Import**.
3. Choose docs/PAYMENT_SERVICE.postman_collection.json.
4. Open the imported **Crypto Payment SDK - Payment Service** collection.
5. Do not send a request until the environment variables below are configured.

## 3. Create a private Postman environment

Create a local environment such as **Payment SDK local manual test**. Define
only configuration and credential variables in that environment:

| Variable | Local value | Secret? |
|---|---|---:|
| ps_base_url | http://127.0.0.1:8081 | no |
| ps_metrics_url | http://127.0.0.1:9091 | no |
| ps_exchange_token | value of PS_API_BEARER_TOKEN | yes |
| ps_admin_token | value of PS_ADMIN_BEARER_TOKEN | yes |
| network | local, or the exact configured PS/IX network | no |
| chain_id | exact active chain ID, commonly 31337 for a local node | no |
| run_id | a new visible identifier, for example tester1-20260806-01 | no |
| allow_high_impact | false | no |
| approved_action | blank; set only for one reviewed high-impact request | no |
| expect_unfunded | true for the initial smoke flow; false after local funding | no |
| user_id | a disposable user, for example postman-user-001 | no |
| asset | native, or an allowlisted lowercase token address | no |
| expected_amount | positive decimal atomic-unit string | no |

Select this environment before sending requests. Keep token values local and
do not export or synchronize the environment.

Do **not** create environment variables named job_id, deposit_id,
collection_id, case_id, or ledger_head_id. The collection's test scripts write
those values at collection scope. An environment variable with the same name
would have higher precedence and could hide a captured value.

The two PS tokens must be different:

- ps_exchange_token calls ordinary exchange routes.
- ps_admin_token calls administrator routes and may also call ordinary routes.

## 4. Understand run_id and idempotency

Every mutation sends an Idempotency-Key derived from run_id.

Use this rule:

- same semantic command and exactly the same body: keep the same run_id;
- any changed body, resource, amount, reason, or new business command: set a
  new run_id.

Idempotency is scoped by authenticated role, operation, and key. Reusing a key
with changed content inside that scope returns HTTP 409. A timeout does not
justify a new command: replay the same request with the same key or poll the
existing job.

Do not use Postman's timestamp or GUID dynamic variable directly in an
idempotency header. It would generate a different key for every retry and make
exact replay impossible.

## 5. Safe smoke-test sequence

Run requests individually in this order.

### A. Preflight

Run these requests from **00 - Health and status**:

1. **Liveness**
   - expected HTTP 200;
   - expected body {"status":"live"}.
2. **Readiness**
   - expected HTTP 200;
   - expected body {"status":"ready","authentication_mode":"strict"}.
3. **Administrator status**
   - expected HTTP 200;
   - service is payment-service;
   - scope is ethereum/local, chain ID 31337 for the launcher defaults;
   - ready, indexer_ready, and wallet_ready are true;
   - event lag is normally "0" for a ready idle stack.
4. **Prometheus metrics**
   - expected HTTP 200;
   - response is Prometheus text from port 9091.

Liveness alone is insufficient. Do not create deposits unless readiness and
administrator status agree that dependencies are ready.

### B. Authentication checks

From **05 - Authentication and routing checks**, run:

1. **Missing token is unauthorized** — expected HTTP 401 and
   WWW-Authenticate: Bearer.
2. **Ordinary token cannot access administrator status** — expected HTTP 403.
3. **Unknown route is not found** — expected HTTP 404 with code
   route_not_found.

These requests are read-only and do not create durable state.

### C. Create one disposable deposit

1. Confirm run_id, user_id, network, asset, and expected_amount.
2. Send **01 - Deposits / Create deposit**.
3. Expect HTTP 202 with job_id and deposit_id.
4. The collection saves both IDs automatically.

This command creates durable PS state, provisions an ephemeral custody key,
and registers an IX watch. It does not fund an address or broadcast a
transaction.

To test exact replay, send **Create deposit** again without changing the body,
token, or run_id. Expect HTTP 202 with exactly the same job and deposit IDs.
The Postman test script compares the replay with the first response.

If you want another deposit, change run_id first. Change user_id as well when
the scenario requires a different user.

### D. Poll the durable job

Run **02 - Jobs / Get job** repeatedly using the automatically captured
job_id.

Valid job states are:

~~~text
queued
running
waiting_retry
succeeded
failed
~~~

For the healthy local scenario, the create-deposit job should eventually be
succeeded. If it is waiting_retry, keep the same job ID, repair the dependency,
and continue polling. Do not submit a replacement create command.

### E. Inspect the deposit

Run these requests:

1. **Get deposit**
2. **List deposits**
3. **Get balances**
4. **Get ledger and capture current head**
5. **Get observations**

Expected before sending any chain funds:

- deposit state is eventually active;
- active deposit has a lowercase 0x Ethereum address and decimal birthday;
- payment_progress is unseen;
- all five balance fields are decimal strings and normally "0";
- ledger contains the initial absolute zero snapshot;
- observations are normally empty.

While a deposit is awaiting_watch, address and birthday must be null and
unavailable. The local worker may activate it too quickly for a tester to
observe that intermediate state.

The ledger request captures ledger_head_id only if the response fits in one
page. If next_cursor is not null, follow every page and determine the
unreferenced ledger entry before using an optimistic ledger head.

## 6. Pagination

The default page size is 100 and the maximum is 1000. The collection uses
page_limit=100.

To continue a page:

1. copy next_cursor from the response;
2. enable the disabled cursor query parameter;
3. paste the cursor;
4. send the request again.

Deposit, collection, reconciliation, and ledger cursors are opaque strings.
Observation cursors are unsigned decimal strings. Do not modify a cursor or
reuse it with another endpoint.

## 7. High-impact manual requests

The collection uses separate blank variables for high-impact targets:

~~~text
approved_action
approved_deposit_id
approved_collection_id
approved_case_id
approved_ledger_head_id
approved_next_accounted
approved_external_reference
~~~

This separation prevents automatically captured smoke-test IDs from being
used accidentally. Set an approved_* value only after reviewing and
authorizing that exact mutation. Clear it again after the test.

High-impact requests also have a pre-request script that blocks transmission
unless all of these are present: a unique run_id, allow_high_impact=true, the
exact request-specific approved_action value, and every required approved_*
target. Keep allow_high_impact false and approved_action blank during ordinary
smoke testing. For one approved mutation:

1. review and set the required approved_* variables;
2. set a new unique run_id;
3. set approved_action to the exact value in the table below;
4. set allow_high_impact to true;
5. send only the reviewed request;
6. immediately set allow_high_impact back to false and clear approved_action
   and the approved_* values.

| Request | approved_action | Required review and effect |
|---|---|---|
| Close deposit | close_deposit | Use a spare deposit with zero balance, no active reservation, and no open reconciliation. The endpoint queues a job with 202; poll the job. The job succeeds only when all close preconditions still hold. |
| Create collection | create_collection | Requires explicit approval, a disposable funded local-chain deposit, and an eligible spendable balance. The workflow may sign and broadcast a transaction. |
| Retry collection | retry_collection | Use only for a reviewed retryable failed or reorged collection. It may lead to signing and broadcasting. |
| Record accounting | account_deposit | Requires administrator approval, the complete current ledger head, and a reviewed absolute next_accounted value. It appends an accounting decision; the amount is not a delta. |
| Reverse credit | resolve_reverse_credit | Requires an open post-credit-reorg case, the reviewed current ledger head, and an approved absolute correction. |
| Accept liability | resolve_accept_liability | Requires an open post-credit-reorg case and explicit approval to retain the liability. |
| Record external debt | resolve_external_debt | Requires an open post-credit-reorg case and a reviewed external accounting reference. |

Only one approved_action value can match during a folder or collection run.
Even with that defense, never use **Run collection** for manual mutation tests;
send the single reviewed request.

Funding a deposit is not a Postman/PS request. It is a separate on-chain
transaction. Never fund an ephemeral custody address with real assets.

### Reconciliation body rules

Choose exactly one resolution request:

- reverse_credit
  - requires approved_ledger_head_id;
  - forbids external_reference.
- accept_liability
  - accepts only resolution and reason.
- external_debt_recorded
  - requires external_reference;
  - forbids expected_ledger_head.

Set a new run_id for the chosen resolution. Do not send the other two
resolution requests afterward.

## 8. Request inventory

| Folder | Requests |
|---|---|
| Health and status | liveness, readiness, metrics, administrator status |
| Deposits | create, list, get, balances, ledger, observations, close |
| Jobs | get job |
| Collections | create, list, get, retry |
| Administrator operations | accounting, list/get reconciliation, three alternative resolution bodies |
| Authentication and routing | missing token, forbidden role, unknown route |

Ordinary routes accept either exchange or administrator credentials. Use the
exchange token for ordinary requests so administrator access remains explicit.

All mutation bodies use strict JSON. Unknown fields are rejected. Monetary
amounts are unsigned decimal strings in atomic units and must never be sent as
JavaScript numbers or floating-point values.

Opaque identifiers contain 1–256 visible ASCII bytes and may not contain
quotes or backslashes. That rule applies to path IDs and to identifiers in
bodies and queries, including user_id, opaque cursors, expected_ledger_head,
and external_reference.

## 9. Expected error behavior

Errors are sanitized JSON objects with:

~~~json
{
  "code": "machine_readable_code",
  "message": "safe description",
  "retryable": false,
  "request_id": "ps-request-..."
}
~~~

Common outcomes:

| Outcome | Meaning/action |
|---|---|
| Connection refused | Process is absent or wrong port is configured. Check listeners with lsof. |
| Address already in use | Another instance owns the port. Reuse the healthy instance or stop the known process with Ctrl-C; do not kill an unknown PID blindly. |
| 401 unauthorized | Token variable is blank, wrong, or the intended Postman environment is not active. |
| 403 forbidden | Exchange token was used on an administrator route. |
| 400 invalid_json | Malformed JSON, an unknown field, or a wrong value type. |
| 400 scope_mismatch | Postman network, PS policy, and IX scope differ. |
| 404 | ID is blank, stale, belongs to another disposable database, or does not exist. |
| 409 | Changed content under the same scoped idempotency key, stale ledger head, or conflicting lifecycle state. |
| 422 | Unsupported asset or invariant-invalid financial command. |
| 503 readiness | PS is reachable but a dependency or projection is not ready. Inspect administrator status and service logs. |
| Job waiting_retry | Repair the unavailable dependency and poll the existing job. |

Before sending, inspect Postman's rendered URL, headers, and body. A variable
placeholder inside a quoted JSON string can remain valid JSON and may be
accepted literally or rejected by field validation; it is not necessarily an
invalid_json error.

Metrics use port 9091, not PS port 8081.

## 10. Finishing a test

Record only sanitized evidence:

- network and chain ID;
- request name and HTTP status;
- resource state and opaque IDs when needed for correlation;
- whether Postman tests passed;
- sanitized error code and request ID for failures.

Do not include tokens, authorization headers, private keys, signed envelopes,
or raw secrets.

Stop foreground services with Ctrl-C. If a terminal was lost, identify the
specific listener first:

~~~bash
lsof -nP -iTCP:8081 -sTCP:LISTEN
~~~

Then send TERM only to the verified PS PID. Stop PS before any offline backup,
migration, or direct database inspection.
