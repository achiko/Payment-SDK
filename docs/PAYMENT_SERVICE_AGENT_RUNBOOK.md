# Payment Service run instruction for another agent

Use this runbook when handing a local Payment Service startup and verification
task to another coding agent. The detailed operator guide and complete curl
catalog are in
[`PAYMENT_SERVICE_USAGE.md`](./PAYMENT_SERVICE_USAGE.md).

## Objective

Start `payment-api` against an already configured Ethereum Indexer Service
(IX), Wallet Service (WS), and custody dependency; prove readiness; run
only the explicitly authorized smoke checks; shut down cleanly; and return
source- and runtime-backed evidence.

Do not claim success because the workspace compiles. Success requires a live
PS process, ready dependencies, `GET /health/ready` returning `200`, and an
authenticated `GET /v1/admin/status` response.

## Safety and scope

- Read `AGENTS.md`, `docs/SYSTEM_REQUIREMENTS.md`, and
  `docs/PAYMENT_SERVICE_USAGE.md` before acting.
- Inspect `git status --short` and preserve all existing changes.
- Treat this as a run-and-verify task. Do not modify Rust source unless the
  handoff explicitly authorizes fixing a discovered blocker.
- Use a disposable local PS database and reviewed non-production policy.
- Never point PS at the IX RocksDB directory or an existing production PS
  database.
- Never print bearer tokens, custody credentials, private keys, seed phrases,
  signed transaction envelopes, or authorization headers.
- Do not run `apps/wallet/examples/live_ethereum_transaction.rs`, broadcast a
  transaction, or fund an address unless the handoff explicitly authorizes
  that exact external action.
- Do not create a deposit or collection against shared/non-disposable IX, WS,
  custody, or chain infrastructure without explicit approval.
- Keep the metrics listener on loopback. Do not expose the PS listener publicly
  without a reviewed TLS-termination setup.

## Local custody option

For disposable local Anvil runs, the repository provides `custody-worker` on
loopback port `8181`. It uses ephemeral in-memory keys and must not be used for
production assets. A production or persistent test deployment still requires
reviewed durable remote custody.

When the handoff requests the complete disposable local stack, prefer the
supervised launcher documented in
[`PAYMENT_SERVICE_USAGE.md`](./PAYMENT_SERVICE_USAGE.md#one-command-local-service-launcher).
It starts IX, custody, WS, and PS, but never starts or stops Anvil or another
Ethereum node.

## Inputs required from the handoff

Obtain or discover these values without printing their secrets:

- Ethereum network name and chain ID;
- reviewed PS policy path;
- disposable PS database path;
- the exact shared `STRICT_AUTHENTICATION_MODE` selection;
- IX URL and strict-mode IX bearer token;
- WS URL and strict-mode WS bearer token;
- distinct ordinary and administrator PS bearer tokens in strict mode; and
- confirmation that IX, WS, and remote custody are non-production or approved
  for the requested test.

Default local listener assumptions are:

```text
IX:      http://127.0.0.1:8080
PS:      http://127.0.0.1:8081
WS:      http://127.0.0.1:8082
metrics: http://127.0.0.1:9091
```

Do not invent working custody credentials or silently switch networks when an
input is unavailable.

## Execution procedure

### 1. Preflight the worktree and build

From the workspace root:

```bash
git status --short
cargo build --locked -p payment-api
./target/debug/payment-api --help
./target/debug/payment-api serve --help
```

Record command exit statuses. Both help commands must succeed before starting
the runtime.

### 2. Verify dependencies

The curl commands below only verify processes that are already running. If a
listener is absent or times out, follow the complete node, IX, custody, WS, and
timeout procedure in
[`PAYMENT_SERVICE_USAGE.md`](./PAYMENT_SERVICE_USAGE.md#step-1-start-and-verify-dependencies).

With the default local listeners:

```bash
curl --fail-with-body --silent --show-error \
  http://127.0.0.1:8080/health/ready

curl --fail-with-body --silent --show-error \
  http://127.0.0.1:8082/health/ready
```

Both must report ready. Independently confirm that the IX scope, WS Ethereum
RPC, and policy refer to the same network. Detail-free health responses alone
do not prove scope equality.

### 3. Prepare disposable local storage

For a local run only:

```bash
PS_AGENT_RUN_ROOT='./tmp/payment-service-agent-run'
mkdir -p "$PS_AGENT_RUN_ROOT/database"
chmod 700 "$PS_AGENT_RUN_ROOT"
```

Save or copy the reviewed policy to
`./tmp/payment-service-agent-run/policy.json`. Do not overwrite a user-owned
policy. The policy format and validation rules are documented in
[`PAYMENT_SERVICE_USAGE.md`](./PAYMENT_SERVICE_USAGE.md#policy-configuration).
For the documented local Anvil run, its scope must use network `anvil` and
chain ID `31337`, with only disposable local addresses and custody locators.

### 4. Configure the process

Set values in the private shell that will run PS. Placeholders are not working
credentials:

```bash
export STRICT_AUTHENTICATION_MODE='true'
export PS_DATABASE_PATH='./tmp/payment-service-agent-run/database'
export PS_POLICY_PATH='./tmp/payment-service-agent-run/policy.json'

export PS_INDEXER_URL='http://127.0.0.1:8080'
export PS_INDEXER_NETWORK='anvil'
export PS_INDEXER_BEARER_TOKEN='replace-with-indexer-token'

export PS_WALLET_URL='http://127.0.0.1:8082'
export PS_WALLET_BEARER_TOKEN='replace-with-wallet-token'

export PS_API_BEARER_TOKEN='replace-with-exchange-token'
export PS_ADMIN_BEARER_TOKEN='replace-with-distinct-admin-token'

export PS_HTTP_BIND='127.0.0.1:8081'
export PS_METRICS_BIND='127.0.0.1:9091'
export RUST_LOG='info'
```

This runbook intentionally selects strict mode. The policy network,
`PS_INDEXER_NETWORK`, and IX scope must match exactly. The ordinary and
administrator PS tokens must be distinct.

### 5. Start Payment Service

```bash
cargo run --locked -p payment-api -- serve
```

Keep this terminal open and capture only non-secret diagnostics. PS owns the
database path exclusively while running.

### 6. Verify liveness, readiness, status, and metrics

In a second private shell with the administrator token available:

```bash
PS_URL='http://127.0.0.1:8081'
PS_METRICS_URL='http://127.0.0.1:9091'

curl --fail-with-body --silent --show-error \
  "$PS_URL/health/live"

curl --fail-with-body --silent --show-error \
  "$PS_URL/health/ready"

curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_ADMIN_BEARER_TOKEN" \
  "$PS_URL/v1/admin/status"

curl --fail-with-body --silent --show-error \
  "$PS_METRICS_URL/metrics"
```

Redact tokens and any sensitive response content from the handoff report.
Readiness must be `200` with
`{"status":"ready","authentication_mode":"strict"}` for this strict-mode
runbook. Record the admin status mode, scope, policy version/digest, IX and WS
readiness flags, ingestion/projection cursors, event lag, and job backlog
without exposing credentials.

### 7. Run only authorized API smoke requests

If the handoff authorizes API mutations against disposable dependencies, use
the complete command catalog in
[`PAYMENT_SERVICE_USAGE.md`](./PAYMENT_SERVICE_USAGE.md#complete-curl-request-catalog).

At minimum:

1. create a deposit with a stable `Idempotency-Key`;
2. poll its returned job ID until terminal;
3. verify that `awaiting_watch` does not expose an address;
4. after success, verify the deposit is `active` and has an address/birthday;
5. read balances, ledger, and observations; and
6. replay the exact create command with the same key and confirm the original
   IDs are returned.

Do not send funds, record accounting, create a collection, resolve a
reconciliation, or close a deposit unless those specific mutations are part of
the authorized test.

### 8. Shut down cleanly

Send `Ctrl-C` to the PS process or let the process manager send `SIGTERM`.
Verify readiness turns off and the process exits within its grace period. Do
not run backup, migration, ingestion, projection inspection, or watch
reconciliation while `serve` still owns the database.

## Required handoff report

Return a concise report containing:

1. current branch/commit and whether the worktree had pre-existing changes;
2. commands executed and their exit statuses;
3. whether the `payment-api serve` command surface parsed successfully;
4. dependency readiness and confirmed Ethereum scope;
5. PS liveness/readiness and sanitized administrator status;
6. any authorized API smoke results, including returned resource states but no
   secrets or signed payloads;
7. shutdown result;
8. files changed, if changes were explicitly authorized; and
9. a clear distinction between compile-time, process-level, and end-to-end
   evidence.

If blocked, report the exact stage, sanitized error text, and the smallest next
action required. Do not replace missing evidence with an implementation claim.
