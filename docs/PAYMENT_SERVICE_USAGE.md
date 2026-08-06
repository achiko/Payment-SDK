# Payment Service usage guide

This guide explains how an exchange backend and an operator use the
single-network Ethereum v1 Payment Service (PS) implemented by `payment-api`.
For ownership rules and persistence semantics, see
[`PAYMENT_SERVICE.md`](./PAYMENT_SERVICE.md). For the canonical system
requirements, see [`SYSTEM_REQUIREMENTS.md`](./SYSTEM_REQUIREMENTS.md).
For a concise coding-agent handoff, use
[`PAYMENT_SERVICE_AGENT_RUNBOOK.md`](./PAYMENT_SERVICE_AGENT_RUNBOOK.md).
For Postman import and a safety-gated manual test sequence, use
[`PAYMENT_SERVICE_POSTMAN.md`](./PAYMENT_SERVICE_POSTMAN.md).

## Local development status

The repository includes all four service processes needed alongside a
disposable local Ethereum node: IX, a loopback-only ephemeral custody adapter,
WS, and PS. The local custody adapter keeps keys only in memory and loses them
when it exits. It is not durable custody and must never be used for production
funds.

## Scope and safety model

One PS process owns:

- one Ethereum network scope;
- one exclusive PS RocksDB directory;
- one active versioned policy;
- one Indexer Service (IX) event feed; and
- one stateless Wallet Service (WS) dependency.

Run a separate PS process and database for every additional network. Never
point PS at the IX database. PS stores public addresses, opaque key locators,
workflow state, and temporarily retained opaque signed envelopes; it does not
store private keys, seed phrases, or custody credentials.

The current v1 flow is for an internal exchange backend. It supports native ETH
and allowlisted standard ERC-20 assets. It does not provide direct end-user
authentication, automatic credit, automatic collection, webhooks, HA, or a
multi-network database.

## Prerequisites

Before PS can become ready, provide:

1. an Ethereum node or JSON-RPC provider used by IX and WS;
2. a running Ethereum Indexer Service for the same network;
3. a remote custody service implementing the signer/provisioner contract, or
   the repository's loopback-only ephemeral adapter for disposable local use;
4. a running stateless Ethereum Wallet Service connected to that custody
   service;
5. a dedicated, encrypted PS data volume; and
6. a reviewed Payment Service policy JSON file.

The recommended startup order is node, IX, custody, WS, then PS. PS readiness
requires IX phase `Ready`, WS readiness, and equal IX-ingestion and PS-projection
cursors.

## Build

From the workspace root:

```bash
cargo build --locked -p custody-worker -p indexer-worker -p wallet-worker -p payment-api
```

Inspect the command surfaces:

```bash
./target/debug/payment-api --help
./target/debug/payment-api serve --help
./target/debug/payment-api backup --help
./target/debug/payment-api migrate --help
./target/debug/payment-api reconcile-watches --help
./target/debug/payment-api ingest-events --help
./target/debug/payment-api projection-status --help
```

Do not interpret a successful build as production readiness. Local Anvil
startup exercises the implemented processes but does not validate production
custody, HA, or a production Ethereum provider.

## Policy configuration

The policy has no financial defaults. Amounts are unsigned decimal strings in
the asset's atomic unit: wei for native ETH and the token's smallest unit for an
ERC-20.

Use only reviewed addresses and limits. The values below are placeholders, not
deployment recommendations:

```json
{
  "version": 1,
  "scope": {
    "chain": "ethereum",
    "network": "sepolia",
    "chain_id": 11155111
  },
  "deposit_ttl_seconds": 86400,
  "assets": [
    {
      "asset": "native",
      "master_destination": "0x1111111111111111111111111111111111111111",
      "minimum_collection_amount": "1000000000000000"
    },
    {
      "asset": "0x2222222222222222222222222222222222222222",
      "master_destination": "0x3333333333333333333333333333333333333333",
      "minimum_collection_amount": "1000000"
    }
  ],
  "fees": {
    "max_fee_per_gas": "100000000000",
    "max_priority_fee_per_gas": "5000000000",
    "max_gas_limit": 200000,
    "max_total_fee": "20000000000000000"
  },
  "gas_funder": {
    "address": "0x4444444444444444444444444444444444444444",
    "key_locator": "replace-with-an-opaque-custody-locator",
    "maximum_funding_amount": "5000000000000000"
  }
}
```

Rules enforced when the policy is loaded:

- the file must not exceed 1 MiB and must contain only the documented fields;
- `version`, `chain_id`, TTL, thresholds, gas limits, and required monetary
  ceilings must be greater than zero where applicable;
- `scope.chain` must be `ethereum`;
- the policy network must exactly match the configured IX network;
- `asset` must be `native` or a lowercase canonical ERC-20 address;
- all Ethereum addresses must be lowercase canonical `0x` hexadecimal;
- duplicate assets are rejected;
- priority fee per gas cannot exceed the maximum fee per gas; and
- the gas-funder key locator must be non-empty.

The complete file bytes determine the policy digest stored with jobs and
collections. Changing formatting changes that digest. A database already bound
to a different scope or policy fails closed rather than reinterpreting durable
work. The API has no asset-discovery route, so configure clients with the
reviewed allowlist and atomic-unit interpretation out of band.

## `serve` environment

For non-disposable environments, prefer a process manager and secret store for
credentials. Do not put bearer values in source control, logs, or command-line
arguments.

| Environment variable | Required | Meaning/default |
|---|---:|---|
| `PS_DATABASE_PATH` | yes | Dedicated PS RocksDB directory; never the IX path |
| `PS_POLICY_PATH` | yes | Versioned policy JSON file |
| `PS_INDEXER_URL` | yes | IX API origin without a path/query/embedded credentials |
| `PS_INDEXER_NETWORK` | yes | Must match the policy network |
| `PS_INDEXER_BEARER_TOKEN` | remote IX | Required for a non-loopback IX endpoint |
| `PS_WALLET_URL` | yes | WS API origin without a path/query/embedded credentials |
| `PS_WALLET_BEARER_TOKEN` | yes | Credential used by PS when calling WS |
| `PS_API_BEARER_TOKEN` | yes | Ordinary exchange-backend credential |
| `PS_ADMIN_BEARER_TOKEN` | yes | Administrator credential; must differ from the ordinary token |
| `PS_HTTP_BIND` | no | `127.0.0.1:8081` |
| `PS_METRICS_BIND` | no | `127.0.0.1:9091`; must remain loopback |
| `PS_TLS_TERMINATED_UPSTREAM` | no | `false`; must be `true` for a non-loopback PS listener |
| `PS_WORKER_INTERVAL_MILLIS` | no | `1000`, allowed range 1–300000 |
| `PS_WORKER_PAGE_SIZE` | no | `100`, allowed range 1–1000 |
| `PS_SHUTDOWN_GRACE_SECONDS` | no | `10`, allowed range 1–300 |

IX client defaults are a 15-second timeout, three attempts, 100 ms initial
backoff, and 1000 ms maximum backoff. Their environment variables are
`PS_INDEXER_TIMEOUT_SECONDS`, `PS_INDEXER_RETRY_ATTEMPTS`,
`PS_INDEXER_RETRY_INITIAL_MILLIS`, and `PS_INDEXER_RETRY_MAX_MILLIS`.

WS uses the same defaults through `PS_WALLET_TIMEOUT_SECONDS`,
`PS_WALLET_RETRY_ATTEMPTS`, `PS_WALLET_RETRY_INITIAL_MILLIS`, and
`PS_WALLET_RETRY_MAX_MILLIS`. Timeouts must be 1–300 seconds, retry attempts
1–10, and initial backoff cannot exceed maximum backoff.

Start PS with:

```bash
cargo run --locked -p payment-api -- serve
```

Plain HTTP dependency URLs are accepted only for `localhost` or loopback IPs.
Remote IX and WS URLs must use HTTPS. A public PS bind additionally requires a
trusted upstream TLS terminator; PS does not terminate TLS itself.

## One-command local service launcher

[`scripts/run-local-payment-services.sh`](../scripts/run-local-payment-services.sh)
starts IX, local ephemeral custody, WS, and PS in dependency order. It does
**not** start or stop Anvil or any other Ethereum node. Start a disposable
loopback Ethereum JSON-RPC node separately, then run from the repository root:

```bash
ETHEREUM_RPC_URL='http://127.0.0.1:8545' \
PAYMENT_NETWORK='local' \
./scripts/run-local-payment-services.sh --disposable-policy
```

The launcher:

- verifies `eth_chainId` and the block-zero hash before starting a service;
- refuses to kill or reuse processes already listening on its ports;
- builds the four packages once and then supervises their direct binaries;
- creates fresh private IX/PS databases under
  `./tmp/payment-sdk-stack.XXXXXX` for every run;
- generates distinct local bearer tokens without printing them;
- provisions a fresh ephemeral gas-funder identity and, only with the explicit
  `--disposable-policy` option, creates a native-only local test policy; and
- waits for IX, custody, WS, and PS readiness before reporting success.

The generated financial limits are documentation placeholders for disposable,
no-funds local testing only. `--disposable-policy` uses
`STACK_MASTER_DESTINATION` when set; otherwise it uses the first lowercase
address returned by the local node's `eth_accounts` response and fails if none
exists. To preserve reviewed assets, destinations, and limits, omit
`--disposable-policy` and set `STACK_POLICY_TEMPLATE` to a policy whose
Ethereum scope, network, and chain ID match the active RPC. The launcher
materializes a runtime policy from that template, changing only the
gas-funder address and locator values to the fresh custody identity. `jq`
serialization can change the file bytes, so the runtime policy digest need not
equal the template's digest.

After startup, the launcher prints the relative path of a mode-`600`
`client.env` file. In another terminal, source that exact path to obtain the
generated PS, WS, IX, and custody credentials. The values themselves are never
printed. Use `--no-build` on later runs only when all four debug binaries are
already current.

Keep the launcher attached. `Ctrl-C` stops PS, WS, custody, and IX in reverse
dependency order but leaves the external Ethereum node untouched. Logs and
databases remain in the printed run directory for diagnosis. Do not reuse its
policy or databases: the corresponding custody keys disappear at shutdown.

Run `./scripts/run-local-payment-services.sh --help` for timeout, bind, policy,
and chain configuration variables.

## Step-by-step: run the service

### Step 1: start and verify dependencies

The health-check commands in this step do **not** start a dependency. They only
contact a process that must already be listening. A timeout therefore normally
means that the corresponding process was never started, exited during startup,
uses another port, or cannot be reached from the current environment.

The local process chain is:

```text
Anvil Ethereum node (:8545)
  ├── Indexer Service (:8080)
  └── Wallet Service (:8082)
        └── local ephemeral custody (:8181)
```

| Dependency | Purpose | Runnable from this repository? |
|---|---|---:|
| Ethereum node | Canonical blocks, receipts, balances, fees, and broadcast | Use local Anvil |
| Indexer Service (IX) | Watches addresses and produces canonical observations | Yes: `indexer-worker` |
| Local custody | Provisions ephemeral keys and signs digests without exposing secrets | Yes: `custody-worker`, local-only |
| Wallet Service (WS) | Stateless Ethereum address/sign/broadcast API | Yes, but only after RPC and custody preflight succeed |

#### 1A. Start a disposable local Ethereum node

Foundry Anvil is appropriate for local testing. In terminal A:

```bash
anvil --host 127.0.0.1 --port 8545 --chain-id 31337
```

Keep it running. In terminal B, verify the node and read the current genesis
hash:

```bash
cast chain-id --rpc-url http://127.0.0.1:8545
cast block 0 --field hash --rpc-url http://127.0.0.1:8545
```

The expected chain ID is `31337`. Capture the printed genesis hash; IX requires
the full `0x`-prefixed 32-byte value and verifies it on startup.

#### 1B. Start Indexer Service

In terminal B, while Anvil is still running:

```bash
mkdir -p ./tmp
IX_LOCAL_ROOT="$(mktemp -d ./tmp/payment-sdk-indexer.XXXXXX)"
IX_GENESIS_HASH="$(cast block 0 \
  --field hash \
  --rpc-url http://127.0.0.1:8545)"

cargo run --locked -p indexer-worker -- serve \
  --database-path "$IX_LOCAL_ROOT/database" \
  --network anvil \
  --bootstrap-height 0 \
  --expected-chain-id 31337 \
  --expected-genesis-hash "$IX_GENESIS_HASH" \
  --rpc-http-url http://127.0.0.1:8545
```

Keep IX running. This command uses the default API bind
`127.0.0.1:8080` and metrics bind `127.0.0.1:9090`. A fresh temporary database
avoids binding a previous IX database to a newly started Anvil genesis block.

In terminal C:

```bash
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8080/health/live

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8080/health/ready
```

A ready local sequence returns `{"status":"ready"}` from the second request.
If IX is listening but catching up or reconciling, readiness may
temporarily return HTTP `503` instead of timing out.

#### 1C. Start local ephemeral custody

In terminal C, start the repository's development-only custody adapter:

```bash
export CUSTODY_BIND='127.0.0.1:8181'
export CUSTODY_BEARER_TOKEN='local-development-token'

cargo run --locked -p custody-worker -- serve
```

Keep this process running. Its keys exist only in memory and are destroyed on
exit, so restart the disposable stack with fresh databases after restarting
custody. The process rejects non-loopback bind addresses.

The adapter provides these authenticated endpoints:

```text
GET  /v1/capabilities
GET  /v1/readiness
POST /v1/keys/provision
POST /v1/keys/public-key
POST /v1/signatures
```

These paths belong to custody, not to PS on port `8081` or WS on port `8082`.
The local adapter defaults to `127.0.0.1:8181`, so the complete URLs are:

```text
http://127.0.0.1:8181/v1/capabilities
http://127.0.0.1:8181/v1/readiness
http://127.0.0.1:8181/v1/keys/provision
http://127.0.0.1:8181/v1/keys/public-key
http://127.0.0.1:8181/v1/signatures
```

Set `WS_CUSTODY_URL` to the origin only. `wallet-worker` appends the paths and
calls them automatically:

```bash
export WS_CUSTODY_URL='http://127.0.0.1:8181'
export WS_CUSTODY_BEARER_TOKEN='local-development-token'
```

To diagnose a **running development custody server**, call its read-only
endpoints directly:

```bash
CUSTODY_URL='http://127.0.0.1:8181'
CUSTODY_TOKEN='local-development-token'

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $CUSTODY_TOKEN" \
  "$CUSTODY_URL/v1/capabilities"

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $CUSTODY_TOKEN" \
  "$CUSTODY_URL/v1/readiness"
```

WS requires capabilities equivalent to:

```json
{
  "curves": ["secp256k1"],
  "schemes": ["ecdsa_secp256k1"],
  "can_sign_messages": false,
  "can_sign_digests": true,
  "requires_user_interaction": false
}
```

and readiness:

```json
{"status":"available"}
```

The three `POST` endpoints mutate or use custody state and normally are called
by WS, not manually by PS operators. On an explicitly disposable development
custody server, their request shapes are:

```bash
# Provision a development key. Save the returned locator for later requests.
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $CUSTODY_TOKEN" \
  --header 'Content-Type: application/json' \
  --data '{
    "operation_id": "local-provision-001",
    "curve": "secp256k1",
    "public_key_format": "uncompressed",
    "purpose": "local-payment-deposit"
  }' \
  "$CUSTODY_URL/v1/keys/provision"
```

After replacing the locator placeholder with the provision response:

```bash
# Read the public key belonging to an opaque locator.
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $CUSTODY_TOKEN" \
  --header 'Content-Type: application/json' \
  --data '{
    "locator": {"kind":"identifier","value":"replace-with-locator"},
    "curve": "secp256k1",
    "format": "uncompressed"
  }' \
  "$CUSTODY_URL/v1/keys/public-key"

# Sign a disposable 32-byte test digest. Never substitute production data.
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $CUSTODY_TOKEN" \
  --header 'Content-Type: application/json' \
  --data '{
    "operation_id": "local-sign-001",
    "locator": {"kind":"identifier","value":"replace-with-locator"},
    "payload": {
      "kind": "digest",
      "bytes_hex": "0x0000000000000000000000000000000000000000000000000000000000000000"
    },
    "scheme": "ecdsa_secp256k1",
    "encoding": "recoverable",
    "key_tweak": null,
    "user_interaction": "not_required"
  }' \
  "$CUSTODY_URL/v1/signatures"
```

Production deployments must replace this adapter with reviewed durable remote
custody. Do not fund keys created by `custody-worker` with real assets.

#### 1D. Start Wallet Service

After custody is running, open terminal D and start WS:

```bash
export WS_ETHEREUM_CHAIN_ID='31337'
export WS_ETHEREUM_RPC_URL='http://127.0.0.1:8545'

export WS_CUSTODY_URL='http://127.0.0.1:8181'
export WS_CUSTODY_BEARER_TOKEN='local-development-token'

export WS_BEARER_TOKEN='local-wallet-token'
export WS_HTTP_BIND='127.0.0.1:8082'
export RUST_LOG='info'

cargo run --locked -p wallet-worker -- serve
```

WS verifies the RPC chain ID, fetches custody capabilities, and checks custody
readiness **before** binding port `8082`. If any preflight fails,
`wallet-worker` exits and the health curl will time out or report connection
failure. Read the foreground WS error first.

Once WS remains running, verify it from another terminal:

```bash
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8082/health/live

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8082/health/ready
```

Do not continue to PS until IX and WS both return HTTP `200` readiness and both
use the same Ethereum network as the PS policy.

#### 1E. Create a disposable gas-funder identity

PS policy validation requires a gas-funder address and opaque custody locator.
Create both through WS; this does not fund the address or broadcast anything:

```bash
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --request POST \
  --header 'Authorization: Bearer local-wallet-token' \
  --header 'Content-Type: application/json' \
  --data '{
    "operation_id": "local-gas-funder-001",
    "asset": {"kind": "native"},
    "key_purpose": "local-gas-funder"
  }' \
  http://127.0.0.1:8082/v1/ethereum/addresses
```

Copy `address` and `key_locator.value` from the response into the policy's
`gas_funder` object. For the disposable master destination, use one address
returned by:

```bash
cast rpc --rpc-url http://127.0.0.1:8545 eth_accounts
```

Do not restart custody after creating the locator. If custody exits, its
in-memory key disappears and the old locator is no longer usable.

#### If a health command times out

First identify whether anything is listening:

```bash
lsof -nP -iTCP:8545 -sTCP:LISTEN
lsof -nP -iTCP:8080 -sTCP:LISTEN
lsof -nP -iTCP:8181 -sTCP:LISTEN
lsof -nP -iTCP:8082 -sTCP:LISTEN
```

Then apply the matching action:

| Result | Meaning | Action |
|---|---|---|
| No listener on `8545` | Ethereum node is not running | Start Anvil and keep its terminal open |
| No listener on `8080` | IX never started or exited | Run IX in the foreground and fix its first error |
| No listener on `8181` | Local custody never started or exited | Run `custody-worker` in the foreground |
| No listener on `8082` | WS never started or failed RPC/custody preflight | Start custody, then run WS in the foreground |
| Curl exit `7` / connection refused | Host/port has no reachable listener | Check the bind address and process terminal |
| Curl exit `28` / timeout | Network access is blocked or a listener did not answer | Retry with the bounded curl above; inspect firewall/sandbox and process logs |
| HTTP `503` with JSON | Process is reachable but not ready | Read its foreground logs; check IX catch-up or WS dependency readiness |
| Address already in use | Another process owns the port | Identify it with `lsof`; do not kill it blindly; choose another bind and update PS URLs |
| `Operation not permitted` | The shell/agent sandbox blocks local sockets | Run in an approved terminal or allow local bind/connect access |

For IX startup errors, check:

```bash
cast chain-id --rpc-url http://127.0.0.1:8545
cast block 0 --field hash --rpc-url http://127.0.0.1:8545
```

The values must match `--expected-chain-id` and
`--expected-genesis-hash`. Use a new disposable IX database when restarting a
fresh Anvil chain.

For WS startup errors, the common order is:

1. wrong or unreachable Ethereum RPC;
2. configured chain ID differs from `eth_chainId`;
3. custody URL has no listener;
4. custody bearer credential is rejected;
5. custody capability response lacks secp256k1 ECDSA digest signing; or
6. custody readiness is not `available`.

The sanitized WS error indicates which preflight stage failed. Increasing curl
timeouts does not fix a process that is absent or exited.

### Step 2: build Payment Service

From the workspace root:

```bash
cargo build --locked -p payment-api
```

### Step 3: prepare local paths

For disposable local testing only, create a private temporary directory. Use an
encrypted durable volume instead of `/tmp` in a real deployment.

```bash
PS_LOCAL_ROOT='./tmp/payment-service-local'
mkdir -p "$PS_LOCAL_ROOT/database"
chmod 700 "$PS_LOCAL_ROOT"
```

Save the reviewed policy JSON from the policy section as:

```text
./tmp/payment-service-local/policy.json
```

For the local Anvil sequence in step 1, change the policy scope to:

```json
"scope": {
  "chain": "ethereum",
  "network": "anvil",
  "chain_id": 31337
}
```

Also replace all policy address and custody-locator placeholders with values
belonging to the disposable local environment. Do not reuse production
destinations or locators.

### Step 4: configure the process

Use distinct token values. The WS token must match the token accepted by the
Wallet Service. The IX token is optional only for an unauthenticated loopback
IX listener.

```bash
export PS_DATABASE_PATH='./tmp/payment-service-local/database'
export PS_POLICY_PATH='./tmp/payment-service-local/policy.json'

export PS_INDEXER_URL='http://127.0.0.1:8080'
export PS_INDEXER_NETWORK='anvil'
# export PS_INDEXER_BEARER_TOKEN='replace-with-indexer-token'

export PS_WALLET_URL='http://127.0.0.1:8082'
export PS_WALLET_BEARER_TOKEN='local-wallet-token'

export PS_API_BEARER_TOKEN='replace-with-exchange-token'
export PS_ADMIN_BEARER_TOKEN='replace-with-distinct-admin-token'

export PS_HTTP_BIND='127.0.0.1:8081'
export PS_METRICS_BIND='127.0.0.1:9091'
export RUST_LOG='info'
```

The `network` field in the policy, `PS_INDEXER_NETWORK`, and the active IX
scope must match exactly. Keep the two PS client tokens distinct.

### Step 5: validate the command surface

Both top-level and `serve` help must work:

```bash
./target/debug/payment-api --help
./target/debug/payment-api serve --help
```

### Step 6: start Payment Service

```bash
cargo run --locked -p payment-api -- serve
```

Keep this process running. It owns the PS RocksDB directory exclusively and
supervises HTTP, metrics, jobs, IX ingestion, projection, reconciliation,
expiration, and collection workers.

### Step 7: verify Payment Service

In another terminal:

```bash
curl --fail-with-body --silent --show-error \
  http://127.0.0.1:8081/health/live

curl --fail-with-body --silent --show-error \
  http://127.0.0.1:8081/health/ready

curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $PS_ADMIN_BEARER_TOKEN" \
  http://127.0.0.1:8081/v1/admin/status
```

Liveness proves only that the process is serving. Do not accept deposits until
readiness returns `200` with `{"status":"ready"}`.

### Step 8: stop Payment Service

Send `SIGINT` with `Ctrl-C` or let the process manager send `SIGTERM`. PS first
turns readiness off, then drains for the configured grace period. Stop it
before any offline backup, migration, ingestion, or reconciliation command
opens the same database.

## Health and monitoring

The service exposes unauthenticated, detail-free health endpoints:

```text
GET /health/live
GET /health/ready
```

Liveness returns `200` while the process is serving. Readiness returns `200`
with `{"status":"ready"}` or `503` with `{"status":"not_ready"}`. Readiness
becomes false before graceful shutdown.

Prometheus metrics are served on the separate loopback metrics listener:

```text
GET http://127.0.0.1:9091/metrics
```

Use the administrator status endpoint for the active policy, dependency
readiness, cursors, projection lag, and bounded job backlog:

```bash
curl --fail-with-body \
  -H "Authorization: Bearer <administrator-token>" \
  http://127.0.0.1:8081/v1/admin/status
```

Runtime logs are structured JSON. Set `RUST_LOG` to the desired tracing filter;
the default filter is `info`.

## HTTP conventions

All `/v1` routes require `Authorization: Bearer ...`. The administrator token
may use every route. The ordinary token receives `403` on administrator-only
routes. Missing or invalid authentication receives `401`.

Every mutation requires an `Idempotency-Key` header. Idempotency is scoped by
authenticated role, operation, and key. Idempotency keys and opaque identifiers
in paths, query parameters, and bodies must contain 1–256 visible ASCII bytes
without quotes or backslashes. Exact command replay returns the original
resource IDs or result; reusing the same scoped key for different semantic
content returns `409`.

Request bodies are limited to 1 MiB. JSON is strict: unknown fields are
rejected. Use:

- unsigned decimal strings for amounts, timestamps, block heights, revisions,
  and other large integers;
- `native` for ETH;
- a lowercase canonical token contract address for an ERC-20; and
- lowercase canonical `0x` Ethereum addresses and transaction IDs.

List endpoints default to 100 records and accept at most 1000. A response with
`next_cursor` is continued by passing that value as `cursor`; pagination is
exclusive-after.

Errors use:

```json
{
  "code": "machine_readable_code",
  "message": "safe contextual message",
  "retryable": false,
  "request_id": "ps-request-..."
}
```

Treat `retryable` as classification, not permission to change an idempotency
key or sign a replacement transaction.

Common responses are `400` for malformed input, `401` for missing or invalid
authentication, `403` for insufficient role or ownership, `404` for an unknown
resource, `409` for idempotency/lifecycle/optimistic-head conflicts, `422` for
an unsupported asset or invariant-invalid command, and retryable `503` for
unavailable storage.

## Deposit workflow

### 1. Create a deposit

The scope and asset must match the active policy. This example requests one ETH
in wei:

```bash
curl --fail-with-body \
  -X POST \
  -H "Authorization: Bearer <ordinary-token>" \
  -H "Idempotency-Key: create-deposit-user-42-001" \
  -H "Content-Type: application/json" \
  --data '{
    "user_id": "user-42",
    "scope": {"chain": "ethereum", "network": "anvil"},
    "asset": "native",
    "expected_amount": "1000000000000000000"
  }' \
  http://127.0.0.1:8081/v1/deposits
```

A successful command returns `202 Accepted`:

```json
{
  "job_id": "job-...",
  "deposit_id": "deposit-..."
}
```

`202` means the durable command was accepted, not that address activation
succeeded. Poll the returned job before treating the deposit as usable.

The address is not available while the deposit is `awaiting_watch`. PS first
captures the IX Ready checkpoint as the birthday, provisions an address through
WS, persists the deposit and zero ledger row, and obtains the durable IX watch
acknowledgement.

### 2. Poll the job

```bash
curl --fail-with-body \
  -H "Authorization: Bearer <ordinary-token>" \
  'http://127.0.0.1:8081/v1/jobs/<job-id>'
```

Job states are `queued`, `running`, `waiting_retry`, `succeeded`, and `failed`.
The response includes the resource ID, attempt count, safe last error, optional
next-attempt time, timestamps, and policy version.

### 3. Read the activated deposit

After the job succeeds:

```bash
curl --fail-with-body \
  -H "Authorization: Bearer <ordinary-token>" \
  'http://127.0.0.1:8081/v1/deposits/<deposit-id>'
```

The response now exposes the canonical address and birthday. Deposit lifecycle
states are `awaiting_watch`, `active`, `expired`, and `closed`. Payment progress
is separate from lifecycle:

| Progress | Meaning |
|---|---|
| `unseen` | No canonical incoming amount |
| `included` | Incoming value exists but none is confirmed |
| `partial` | Confirmed value is below the expected amount |
| `paid` | Confirmed value equals the expected amount |
| `overpaid` | Confirmed value exceeds the expected amount |

Expiration does not remove the IX watch or hide late payments.

### 4. Read balances and history

```bash
curl --fail-with-body \
  -H "Authorization: Bearer <ordinary-token>" \
  'http://127.0.0.1:8081/v1/deposits/<deposit-id>/balances'

curl --fail-with-body \
  -H "Authorization: Bearer <ordinary-token>" \
  'http://127.0.0.1:8081/v1/deposits/<deposit-id>/ledger?limit=100'

curl --fail-with-body \
  -H "Authorization: Bearer <ordinary-token>" \
  'http://127.0.0.1:8081/v1/deposits/<deposit-id>/observations?limit=100'
```

Balances are complete absolute snapshots:

```json
{
  "received": "1000000000000000000",
  "confirmed": "1000000000000000000",
  "balance": "1000000000000000000",
  "collected": "0",
  "accounted": "0"
}
```

`received` and `confirmed` may decrease after a reorg. Historical ledger rows
remain immutable. `accounted` changes only through the administrator accounting
command, and `collected` advances only after a confirmed PS-owned sweep.

## Explicit accounting

Crediting a user's exchange account is a separate administrator decision. The
API does not expose a dedicated current-head field, and ledger pages are not a
promise of chronological order. Follow every page and use the immutable links:
the current head is the entry whose `ledger_entry_id` is not referenced by any
other entry's `previous_ledger_entry_id`. Use that ID as the optimistic
`expected_ledger_head`.

```bash
curl --fail-with-body \
  -X POST \
  -H "Authorization: Bearer <administrator-token>" \
  -H "Idempotency-Key: account-deposit-001" \
  -H "Content-Type: application/json" \
  --data '{
    "next_accounted": "1000000000000000000",
    "expected_ledger_head": "<latest-ledger-entry-id>",
    "reason": "Credit confirmed exchange deposit"
  }' \
  'http://127.0.0.1:8081/v1/deposits/<deposit-id>/accounting'
```

`next_accounted` is an absolute value, not a delta. A positive credit cannot
exceed the currently confirmed value. A stale ledger head fails with a
conflict. Reload the complete ledger and reconsider the business decision. If
the command is still valid, submit its revised head under a new idempotency key
because its semantic content changed.

## Collection workflow

Collection is explicit. PS derives the destination, mode, and amount from the
policy and current eligible balance; callers do not supply transaction fees,
destinations, or key locators.

```bash
curl --fail-with-body \
  -X POST \
  -H "Authorization: Bearer <ordinary-token>" \
  -H "Idempotency-Key: collect-deposit-001" \
  -H "Content-Type: application/json" \
  --data '{"deposit_id":"<deposit-id>"}' \
  http://127.0.0.1:8081/v1/collections
```

The response is `202 Accepted` with `job_id` and `collection_id`. This records
the durable command; it does not mean funds moved. Poll the job, then inspect
the collection:

```bash
curl --fail-with-body \
  -H "Authorization: Bearer <ordinary-token>" \
  'http://127.0.0.1:8081/v1/collections/<collection-id>'
```

Native ETH uses one `sweep` leg. ERC-20 uses a `gas_funding` leg when the
deposit address lacks native gas, waits for IX confirmation, and then advances
the token `sweep` leg. Leg states are `required`, `signed`, `broadcast`,
`confirmed`, `failed`, and `reorged`.

PS persists the expected transaction ID and exact signed envelope before the
first broadcast attempt. After a lost response it retries those bytes and
requires the provider's transaction ID to match. Do not manually replace or
fee-bump a PS-owned transaction in v1.

Failed or reorged collections require an explicit retry with a new
idempotency key:

```bash
curl --fail-with-body \
  -X POST \
  -H "Authorization: Bearer <ordinary-token>" \
  -H "Idempotency-Key: retry-collection-001" \
  'http://127.0.0.1:8081/v1/collections/<collection-id>/retry'
```

## Reconciliation after a post-credit reorg

If a reorg reduces `confirmed` below an already credited `accounted` amount,
PS preserves the historical accounting decision, opens a blocking
reconciliation case, and blocks further automated workflow for the deposit.

List open cases with the administrator token:

```bash
curl --fail-with-body \
  -H "Authorization: Bearer <administrator-token>" \
  'http://127.0.0.1:8081/v1/reconciliations?state=open&limit=100'
```

Choose exactly one reviewed business resolution:

- `reverse_credit`: requires `expected_ledger_head` and appends an absolute
  accounting correction;
- `accept_liability`: preserves `accounted`; or
- `external_debt_recorded`: preserves `accounted` and requires an opaque
  `external_reference`.

Example reverse-credit request:

```bash
curl --fail-with-body \
  -X POST \
  -H "Authorization: Bearer <administrator-token>" \
  -H "Idempotency-Key: resolve-reconciliation-001" \
  -H "Content-Type: application/json" \
  --data '{
    "resolution": "reverse_credit",
    "expected_ledger_head": "<latest-ledger-entry-id>",
    "reason": "Reverse credit after canonical reorg"
  }' \
  'http://127.0.0.1:8081/v1/reconciliations/<case-id>/resolve'
```

Do not include `external_reference` for this resolution. Conversely,
`external_debt_recorded` requires it and does not accept
`expected_ledger_head`.

## Complete curl request catalog

This catalog covers all 17 authenticated Payment Service operations plus
liveness, readiness, and metrics. Commands are formatted for a POSIX-compatible
shell. Run mutations only when their lifecycle preconditions are satisfied;
the close, retry, accounting, and reconciliation examples are not a sequence
that should be executed blindly against one deposit.

### Set reusable client variables

Use the same client tokens configured on the running PS process. Replace
resource IDs after receiving them from earlier responses:

~~~bash
PS_URL='http://127.0.0.1:8081'
PS_METRICS_URL='http://127.0.0.1:9091'
PS_EXCHANGE_TOKEN='replace-with-exchange-token'
PS_ADMIN_TOKEN='replace-with-admin-token'
PS_NETWORK='anvil'
PS_USER_ID='user-42'
PS_EXPECTED_AMOUNT='1000000000000000000'

JOB_ID='replace-with-job-id'
DEPOSIT_ID='replace-with-deposit-id'
COLLECTION_ID='replace-with-collection-id'
CASE_ID='replace-with-reconciliation-case-id'
LEDGER_HEAD_ID='replace-with-current-ledger-head-id'
~~~

All mutation keys below are examples. Use a stable unique key for one semantic
command, replay that same key only for the exact same command, and use a new key
for a genuinely new command.

### Health and metrics

#### Liveness

~~~bash
curl --fail-with-body --silent --show-error \
  "$PS_URL/health/live"
~~~

#### Readiness

~~~bash
curl --fail-with-body --silent --show-error \
  "$PS_URL/health/ready"
~~~

#### Prometheus metrics

~~~bash
curl --fail-with-body --silent --show-error \
  "$PS_METRICS_URL/metrics"
~~~

### Deposit requests

#### Create a deposit — POST /v1/deposits

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --header "Idempotency-Key: create-deposit-$PS_USER_ID-001" \
  --header 'Content-Type: application/json' \
  --data @- \
  "$PS_URL/v1/deposits" <<JSON
{
  "user_id": "$PS_USER_ID",
  "scope": {
    "chain": "ethereum",
    "network": "$PS_NETWORK"
  },
  "asset": "native",
  "expected_amount": "$PS_EXPECTED_AMOUNT"
}
JSON
~~~

Copy `job_id` and `deposit_id` from the `202 Accepted` response into `JOB_ID`
and `DEPOSIT_ID`. Do not expose the deposit address until the job succeeds and
the deposit becomes `active`.

For ERC-20, replace `native` with a lowercase allowlisted contract address and
express `expected_amount` in that token's smallest unit.

#### List deposits — GET /v1/deposits

~~~bash
curl --fail-with-body --silent --show-error \
  --get \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --data-urlencode "user_id=$PS_USER_ID" \
  --data-urlencode 'state=active' \
  --data-urlencode 'limit=100' \
  "$PS_URL/v1/deposits"
~~~

Valid state filters are `awaiting_watch`, `active`, `expired`, and `closed`. To
continue a page, add `--data-urlencode "cursor=<next_cursor>"`.

#### Get one deposit — GET /v1/deposits/{deposit_id}

~~~bash
curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  "$PS_URL/v1/deposits/$DEPOSIT_ID"
~~~

#### Get balances — GET /v1/deposits/{deposit_id}/balances

~~~bash
curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  "$PS_URL/v1/deposits/$DEPOSIT_ID/balances"
~~~

#### Get ledger entries — GET /v1/deposits/{deposit_id}/ledger

~~~bash
curl --fail-with-body --silent --show-error \
  --get \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --data-urlencode 'limit=100' \
  "$PS_URL/v1/deposits/$DEPOSIT_ID/ledger"
~~~

Follow every `next_cursor` when deriving the current ledger head. The head is
the entry whose `ledger_entry_id` is not referenced by another entry's
`previous_ledger_entry_id`.

#### Get observations — GET /v1/deposits/{deposit_id}/observations

~~~bash
curl --fail-with-body --silent --show-error \
  --get \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --data-urlencode 'limit=100' \
  "$PS_URL/v1/deposits/$DEPOSIT_ID/observations"
~~~

To continue from an event cursor, add
`--data-urlencode "cursor=<decimal-next-cursor>"`.

#### Close a deposit — POST /v1/deposits/{deposit_id}/close

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --header "Idempotency-Key: close-deposit-$DEPOSIT_ID-001" \
  "$PS_URL/v1/deposits/$DEPOSIT_ID/close"
~~~

The endpoint accepts and queues a close job with HTTP `202`. That job succeeds
only at an exact zero-balance ledger head with no active reservation and no
open reconciliation. Closing retains the IX watch.

### Job request

#### Get a job — GET /v1/jobs/{job_id}

~~~bash
curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  "$PS_URL/v1/jobs/$JOB_ID"
~~~

Poll durable jobs until `succeeded` or `failed`. A `waiting_retry` job should
keep its existing ID.

### Collection requests

#### Create a collection — POST /v1/collections

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --header "Idempotency-Key: create-collection-$DEPOSIT_ID-001" \
  --header 'Content-Type: application/json' \
  --data @- \
  "$PS_URL/v1/collections" <<JSON
{
  "deposit_id": "$DEPOSIT_ID"
}
JSON
~~~

Copy the returned `job_id` and `collection_id` into `JOB_ID` and
`COLLECTION_ID`, then poll the job.

#### List collections — GET /v1/collections

~~~bash
curl --fail-with-body --silent --show-error \
  --get \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --data-urlencode "deposit_id=$DEPOSIT_ID" \
  --data-urlencode 'limit=100' \
  "$PS_URL/v1/collections"
~~~

#### Get one collection — GET /v1/collections/{collection_id}

~~~bash
curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  "$PS_URL/v1/collections/$COLLECTION_ID"
~~~

#### Retry a collection — POST /v1/collections/{collection_id}/retry

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_EXCHANGE_TOKEN" \
  --header "Idempotency-Key: retry-collection-$COLLECTION_ID-001" \
  "$PS_URL/v1/collections/$COLLECTION_ID/retry"
~~~

Use retry only for a collection in a retryable failed or reorged lifecycle
state. The response provides the durable job to poll.

### Administrator requests

#### Record accounting — POST /v1/deposits/{deposit_id}/accounting

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_ADMIN_TOKEN" \
  --header "Idempotency-Key: account-deposit-$DEPOSIT_ID-001" \
  --header 'Content-Type: application/json' \
  --data @- \
  "$PS_URL/v1/deposits/$DEPOSIT_ID/accounting" <<JSON
{
  "next_accounted": "$PS_EXPECTED_AMOUNT",
  "expected_ledger_head": "$LEDGER_HEAD_ID",
  "reason": "Credit confirmed exchange deposit"
}
JSON
~~~

`next_accounted` is an absolute amount, not a delta. Refresh the complete
ledger and use a new idempotency key if the optimistic head conflicts and the
revised business command remains valid.

#### List reconciliation cases — GET /v1/reconciliations

~~~bash
curl --fail-with-body --silent --show-error \
  --get \
  --header "Authorization: Bearer $PS_ADMIN_TOKEN" \
  --data-urlencode 'state=open' \
  --data-urlencode 'limit=100' \
  "$PS_URL/v1/reconciliations"
~~~

The default and `state=open` return open cases. Use `state=all` to include
resolved cases. An optional `deposit_id` filter is also accepted.

#### Get one reconciliation case — GET /v1/reconciliations/{case_id}

~~~bash
curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_ADMIN_TOKEN" \
  "$PS_URL/v1/reconciliations/$CASE_ID"
~~~

#### Resolve a reconciliation — POST /v1/reconciliations/{case_id}/resolve

Use exactly one of the following three request bodies.

##### Reverse credit

`reverse_credit` requires the current ledger head and forbids
`external_reference`.

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_ADMIN_TOKEN" \
  --header "Idempotency-Key: reverse-credit-$CASE_ID-001" \
  --header 'Content-Type: application/json' \
  --data @- \
  "$PS_URL/v1/reconciliations/$CASE_ID/resolve" <<JSON
{
  "resolution": "reverse_credit",
  "expected_ledger_head": "$LEDGER_HEAD_ID",
  "reason": "Reverse credit after canonical reorg"
}
JSON
~~~

##### Accept liability

`accept_liability` accepts only a reason.

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_ADMIN_TOKEN" \
  --header "Idempotency-Key: accept-liability-$CASE_ID-001" \
  --header 'Content-Type: application/json' \
  --data @- \
  "$PS_URL/v1/reconciliations/$CASE_ID/resolve" <<JSON
{
  "resolution": "accept_liability",
  "reason": "Business accepts the post-reorg liability"
}
JSON
~~~

##### Record external debt

`external_debt_recorded` requires an opaque external reference and forbids
`expected_ledger_head`.

~~~bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $PS_ADMIN_TOKEN" \
  --header "Idempotency-Key: external-debt-$CASE_ID-001" \
  --header 'Content-Type: application/json' \
  --data @- \
  "$PS_URL/v1/reconciliations/$CASE_ID/resolve" <<JSON
{
  "resolution": "external_debt_recorded",
  "external_reference": "debt-case-9001",
  "reason": "Debt recorded in the external accounting system"
}
JSON
~~~

#### Get administrator status — GET /v1/admin/status

~~~bash
curl --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_ADMIN_TOKEN" \
  "$PS_URL/v1/admin/status"
~~~

The administrator token is a superset and may call ordinary routes. Use the
ordinary token for normal exchange traffic so administrator access remains
explicit and auditable.

## Offline maintenance commands

Every maintenance command opens the PS RocksDB directory as its exclusive
owner. Stop `payment-api serve` before running one; do not run two commands
against the same path concurrently.

### Create and verify a backup

```bash
cargo run --locked -p payment-api -- backup \
  --database-path /absolute/path/to/payment-db \
  --backup-path /absolute/path/to/payment-backups
```

The backup path must be a dedicated directory, cannot be filesystem root, and
must not equal, contain, or be contained by the live database path.

### Migrate and bind a database

Migration creates and verifies a physical backup before mutation, validates
semantic records and references, rebuilds supplementary indexes, and then
binds the database to the policy and Ethereum scope.

```bash
cargo run --locked -p payment-api -- migrate \
  --database-path /absolute/path/to/payment-db \
  --backup-path /absolute/path/to/pre-migration-backup \
  --policy-path /absolute/path/to/payment-policy.json \
  --network anvil
```

`--network` must exactly match the policy. Stop every process using the
database. For rollback, restore the verified backup into a new directory,
validate it, and switch `PS_DATABASE_PATH`; do not overwrite the old directory
in place. There is currently no `payment-api restore` subcommand.

### Resume `AwaitingWatch` deposits

This command never provisions a replacement key. It retries only the durable
IX acknowledgement using the already persisted address and birthday:

```bash
PS_INDEXER_BEARER_TOKEN='<indexer-token>' \
  cargo run --locked -p payment-api -- reconcile-watches \
  --database-path /absolute/path/to/payment-db \
  --indexer-url https://indexer.example.invalid \
  --network anvil \
  --page-size 100 \
  --max-batches 100
```

Omit the bearer environment variable only for an unauthenticated loopback IX
deployment.

### Mirror a bounded IX event backlog

```bash
PS_INDEXER_BEARER_TOKEN='<indexer-token>' \
  cargo run --locked -p payment-api -- ingest-events \
  --database-path /absolute/path/to/payment-db \
  --indexer-url https://indexer.example.invalid \
  --network anvil \
  --page-size 100 \
  --max-pages 100
```

This command advances only the durable ingestion cursor. It does not run the
business projection worker.

### Inspect projection backlog

```bash
cargo run --locked -p payment-api -- projection-status \
  --database-path /absolute/path/to/payment-db \
  --sample-limit 100
```

The command reports ingestion and projection cursors plus a bounded pending
sample. During normal service operation, use `/v1/admin/status` instead.

## Recovery rules

- `waiting_retry` jobs are durable. Poll their existing job IDs; do not create
  replacement commands with new idempotency keys merely because a dependency
  timed out.
- An `awaiting_watch` deposit is not safe to expose. Let the supervised worker
  or offline `reconcile-watches` finish the original IX handshake.
- A broadcast timeout has an unknown outcome. PS must rebroadcast the persisted
  exact envelope and verify the expected transaction ID, never re-sign as a
  fresh transaction.
- A projection backlog makes readiness false. Inspect admin status and the
  unresolved event rather than moving the projection cursor manually.
- IX and PS histories are append-only. Reorg recovery appends corrections; do
  not delete previous events or ledger rows.
- Restore backups to a new directory and switch configuration only after
  validation.

## Current limitations

- The included custody server is ephemeral and loopback-only; durable custody
  remains an external production responsibility.
- The launcher and manual startup sequence are documented, but no automated
  checked-in scenario proves the composed path or production behavior.
- IX-driven collection-leg changes and the corresponding ledger/projection
  update are replay-safe but are not yet one physical PS storage batch.
- The complete crash-window and collection-workflow test matrix is unfinished.
- Ethereum v1 does not detect mempool replacement/drop behavior, index internal
  native transfers, or support nonstandard fee-on-transfer/rebasing tokens.
- Bitcoin Payment Service workflows, automatic credit/collection, webhooks,
  HA, fee replacement, and a multi-network PS database are excluded.
