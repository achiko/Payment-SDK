# Run the local payment stack manually with `.env`

This guide starts Anvil, Indexer Service (IX), ephemeral custody, Wallet
Service (WS), and Payment Service (PS) one process at a time. It does not use
`run-local-payment-services.sh`, and it does not start anything automatically.
Complete and verify each step before moving to the next one.

This setup is disposable local development only. Custody stores keys in memory,
so stopping custody destroys every key behind the run's locators. Never send
real funds to an address created by this stack.

## How the `.env` file works

The Rust binaries already read environment variables through their command-line
configuration, but they do **not** automatically open a `.env` file. The file
works because every terminal sources it before starting a process:

```zsh
source ./.env
```

The tracked [`.env.example`](../../.env.example) contains `export` assignments,
so one `source` command exports all values to the process started from that
terminal. Source it again in every new terminal and after every edit. Running
`./.env` as a program will not configure the current terminal.

The template contains every input required by this manual local flow. Optional
retry, timeout, fee-safety, and shutdown tuning keeps each binary's documented
default instead of duplicating every default in the file.

Treat `.env` as shell code: source only the local file you reviewed. The real
`.env` is ignored by Git; `.env.example` contains no credentials and remains
trackable. Do not put `$(openssl ...)`, `$(cast ...)`, backticks, or other
commands in `.env`. Values must stay fixed so every service receives the same
tokens and chain identity.

The Payment Service policy cannot be flattened into `.env`. PS validates a
versioned JSON policy and binds its exact file bytes to durable work through a
policy digest. `.env` therefore stores `PS_POLICY_PATH`; step 6 creates the
separate private JSON file.

## Prerequisites

Run commands from the repository root. The guide requires Rust/Cargo, Foundry
(`anvil` and `cast`), `curl`, `jq`, `openssl`, `rg`, and `lsof`.

Build all four Rust processes once. Starting the built binaries directly avoids
several foreground `cargo run` processes competing for Cargo's build lock:

```zsh
cargo build --locked \
  -p indexer-worker \
  -p custody-worker \
  -p wallet-worker \
  -p payment-api
```

## Step 1: create and secure `.env`

Create the private working copy:

```zsh
cp .env.example .env
chmod 600 .env
```

Edit `.env` and fill these empty values:

- `LOCAL_RUN_ID`: a new identifier for this Anvil and custody lifecycle, such
  as `anvil-20260807-01`; use letters, digits, and hyphens only.
- `IX_BEARER_TOKEN`
- `CUSTODY_BEARER_TOKEN`
- `WS_BEARER_TOKEN`
- `PS_API_BEARER_TOKEN`
- `PS_ADMIN_BEARER_TOKEN`

Generate one fresh 32-byte hexadecimal value at a time and paste it into one
token field. Run this command five times; do not paste its output into chat,
logs, or source control:

```zsh
openssl rand -hex 32
```

The ordinary `PS_API_BEARER_TOKEN` and `PS_ADMIN_BEARER_TOKEN` must be
different. The file derives the cross-service copies automatically:

```text
IX_BEARER_TOKEN       = PS_INDEXER_BEARER_TOKEN
CUSTODY_BEARER_TOKEN  = WS_CUSTODY_BEARER_TOKEN
WS_BEARER_TOKEN       = PS_WALLET_BEARER_TOKEN
```

Source and inspect the configuration without printing any secret value:

```zsh
source ./.env

# Before Anvil starts, this must print only IX_EXPECTED_GENESIS_HASH.
rg -n "^export [A-Z][A-Z0-9_]*=''$" .env

if [[ "$PS_API_BEARER_TOKEN" == "$PS_ADMIN_BEARER_TOKEN" ]]; then
  echo 'PS API and administrator tokens must be different'
fi
```

Create fresh private runtime directories:

```zsh
umask 077
mkdir -p \
  "$IX_DATABASE_PATH" \
  "$PS_DATABASE_PATH" \
  "$(dirname "$PS_POLICY_PATH")"
chmod 700 \
  "$LOCAL_RUN_ROOT" \
  "$(dirname "$IX_DATABASE_PATH")" \
  "$(dirname "$PS_DATABASE_PATH")"
```

Checkpoint: only `IX_EXPECTED_GENESIS_HASH` remains empty, the two PS tokens
differ, and the new run directories are under `./tmp`.

## Step 2: start Anvil and record its identity

In terminal A:

```zsh
anvil
```

Keep Anvil running. In a separate setup/check terminal, source `.env` and read
the active chain identity:

```zsh
source ./.env

cast chain-id --rpc-url "$ETHEREUM_RPC_URL"
cast block 0 --field hash --rpc-url "$ETHEREUM_RPC_URL"
```

The chain ID must be `31337`. Copy the complete block-zero hash into
`IX_EXPECTED_GENESIS_HASH` in `.env`, then re-source and verify it without
printing secrets:

```zsh
source ./.env

# This command must now print nothing.
rg -n "^export [A-Z][A-Z0-9_]*=''$" .env

test "$(cast chain-id --rpc-url "$ETHEREUM_RPC_URL")" = \
  "$IX_EXPECTED_CHAIN_ID"
test "$(cast block 0 --field hash --rpc-url "$ETHEREUM_RPC_URL")" = \
  "$IX_EXPECTED_GENESIS_HASH"
```

The `.env` entries supply the same values as the IX command-line options we
used manually earlier:

| `.env` entry | Equivalent option | Meaning |
|---|---|---|
| `IX_NETWORK=anvil` | `--network anvil` | Operator-chosen logical scope name shared by IX, PS, and the policy; it is not discovered from RPC. |
| `IX_BOOTSTRAP_HEIGHT=0` | `--bootstrap-height 0` | First block for this fresh disposable IX database. |
| `IX_EXPECTED_CHAIN_ID=31337` | `--expected-chain-id 31337` | Expected EVM chain ID returned by RPC. |
| `IX_EXPECTED_GENESIS_HASH=0x...` | `--expected-genesis-hash 0x...` | Exact block-zero identity of this Anvil chain. |

The network label separates business/deployment scopes, while the chain ID and
genesis hash prove which concrete chain the RPC exposes. Bootstrap height zero
tells a **fresh** IX database to begin at the Anvil genesis block. These values
become part of the persisted IX scope, so do not reuse the directory after
restarting Anvil.

Checkpoint: both `test` commands exit successfully.

## Step 3: start and verify Indexer Service

In terminal B:

```zsh
source ./.env
./target/debug/indexer-worker serve
```

Keep IX running. In the setup/check terminal:

```zsh
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8080/health/live

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8080/health/ready
```

Checkpoint: readiness returns HTTP 200 with `{"status":"ready"}`. A 503 means
IX is reachable but not ready yet; inspect terminal B.

## Step 4: start and verify ephemeral custody

In terminal C:

```zsh
source ./.env
./target/debug/custody-worker serve
```

Keep custody running. In the setup/check terminal:

```zsh
source ./.env

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $CUSTODY_BEARER_TOKEN" \
  http://127.0.0.1:8181/v1/capabilities

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $CUSTODY_BEARER_TOKEN" \
  http://127.0.0.1:8181/v1/readiness
```

Checkpoint: readiness returns `{"status":"available"}`. This custody adapter
accepts only loopback binds and keeps keys and idempotency state in process
memory. If it stops, discard this run's policy and both databases and begin a
new run with a new `LOCAL_RUN_ID`.

## Step 5: start and verify Wallet Service

In terminal D:

```zsh
source ./.env
./target/debug/wallet-worker serve
```

WS verifies the Ethereum chain and checks custody capabilities and readiness
before binding its HTTP port. Keep it running. In the setup/check terminal:

```zsh
curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8082/health/live

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8082/health/ready
```

Checkpoint: readiness returns HTTP 200 with `{"status":"ready"}`.

## Step 6: create the gas-funder identity and policy

Run this step in the setup/check terminal. It asks WS to provision a disposable
gas-funder identity in the currently running custody process. It does not fund
the address and does not expose a private key:

```zsh
source ./.env
umask 077

if curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --request POST \
  --header "Authorization: Bearer $WS_BEARER_TOKEN" \
  --header 'Content-Type: application/json' \
  --data "$(jq --compact-output --null-input \
    --arg operation_id "${LOCAL_RUN_ID}-gas-funder" \
    '{
      operation_id: $operation_id,
      asset: {kind: "native"},
      key_purpose: "local-gas-funder"
    }')" \
  --output "$GAS_FUNDER_RESPONSE_PATH" \
  http://127.0.0.1:8082/v1/ethereum/addresses
then
  chmod 600 "$GAS_FUNDER_RESPONSE_PATH"
  jq --exit-status \
    '(.address | test("^0x[0-9a-f]{40}$")) and
     (.key_locator.kind == "identifier") and
     (.key_locator.value | type == "string" and length > 0)' \
    "$GAS_FUNDER_RESPONSE_PATH" >/dev/null
fi
```

Create the native-ETH-only disposable Anvil policy. The master destination is
Anvil's first local account; the gas-funder address and opaque locator come
from the current custody lifecycle:

```zsh
jq --null-input \
  --arg network "$PAYMENT_NETWORK" \
  --argjson chain_id "$ETHEREUM_CHAIN_ID" \
  --arg master_destination "$(
    cast rpc --rpc-url "$ETHEREUM_RPC_URL" eth_accounts |
      jq --exit-status --raw-output '.[0] | ascii_downcase'
  )" \
  --arg gas_funder_address "$(
    jq --exit-status --raw-output '.address | ascii_downcase' \
      "$GAS_FUNDER_RESPONSE_PATH"
  )" \
  --arg gas_funder_locator "$(
    jq --exit-status --raw-output '.key_locator.value' \
      "$GAS_FUNDER_RESPONSE_PATH"
  )" \
  '{
    version: 1,
    scope: {chain: "ethereum", network: $network, chain_id: $chain_id},
    deposit_ttl_seconds: 86400,
    assets: [{
      asset: "native",
      master_destination: $master_destination,
      minimum_collection_amount: "1000000000000000"
    }],
    fees: {
      max_fee_per_gas: "100000000000",
      max_priority_fee_per_gas: "5000000000",
      max_gas_limit: 200000,
      max_total_fee: "20000000000000000"
    },
    gas_funder: {
      address: $gas_funder_address,
      key_locator: $gas_funder_locator,
      maximum_funding_amount: "5000000000000000"
    }
  }' >"$PS_POLICY_PATH"

chmod 600 "$PS_POLICY_PATH"
jq --exit-status \
  --arg network "$PAYMENT_NETWORK" \
  --argjson chain_id "$ETHEREUM_CHAIN_ID" \
  '.scope == {chain: "ethereum", network: $network, chain_id: $chain_id}' \
  "$PS_POLICY_PATH" >/dev/null
```

These limits copy the repository's disposable local policy; they are not
production recommendations. Do not edit the policy after PS has bound its
database to the policy digest.

Checkpoint: both `jq --exit-status` validations succeed and the policy file
exists with mode 600.

## Step 7: start and verify Payment Service

In terminal E:

```zsh
source ./.env
./target/debug/payment-api serve
```

Keep PS running. In the setup/check terminal:

```zsh
source ./.env

curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  http://127.0.0.1:8081/health/ready

if curl --connect-timeout 2 --max-time 10 \
  --fail-with-body --silent --show-error \
  --header "Authorization: Bearer $PS_ADMIN_BEARER_TOKEN" \
  --output "$PS_ADMIN_STATUS_PATH" \
  http://127.0.0.1:8081/v1/admin/status
then
  chmod 600 "$PS_ADMIN_STATUS_PATH"
  jq '{
    service,
    scope,
    ready,
    indexer_ready,
    wallet_ready,
    event_lag,
    job_backlog
  }' "$PS_ADMIN_STATUS_PATH"
fi
```

Checkpoint: health returns `{"status":"ready"}` and administrator status
reports `ready`, `indexer_ready`, and `wallet_ready` as `true`, with network
`anvil` and chain ID `31337`.

## Step 8: stop the stack

Press `Ctrl-C` in each foreground terminal in reverse dependency order:

1. Payment Service — terminal E
2. Wallet Service — terminal D
3. custody — terminal C
4. Indexer Service — terminal B
5. Anvil — terminal A

PS and IX own separate RocksDB directories. Stop the owning process before an
offline backup or migration opens its database. Once custody stops, do not
reuse this run's databases, gas-funder response, or policy with another custody
process.

## Troubleshooting

- `401 Unauthorized` means the presented bearer token is missing or not known
  to that service. Re-source the same `.env` in the client terminal.
- `403 Forbidden` on `/v1/admin/status` usually means the ordinary PS API token
  was valid but lacks administrator scope. Use `PS_ADMIN_BEARER_TOKEN`.
- `bearer token must be non-empty and contain no whitespace` means a token is
  blank, contains a copied newline/space, or the terminal did not source the
  completed `.env`.
- `ordinary and administrator bearer tokens must be different` means the two
  PS client values in `.env` are identical.
- Editing `.env` does not change a process that is already running. Re-source
  it and restart only the affected process, unless custody or chain identity
  changed; those changes require a completely fresh run.
- `network` must be exactly `anvil` in IX, `PS_INDEXER_NETWORK`, and the policy.
  Chain ID must be `31337` in IX, WS, and the policy.
- A fresh Anvil can have a different genesis hash. Use a new `LOCAL_RUN_ID`,
  fresh databases, and the new block-zero hash.
- Check occupied ports with `lsof -nP -iTCP:<port> -sTCP:LISTEN`. This guide
  uses `8545`, `8080`, `9090`, `8181`, `8082`, `8081`, and `9091`. Do not kill
  an unknown listener blindly.

For the complete API catalog and operational details, see
[`docs/PAYMENT_SERVICE_USAGE.md`](../PAYMENT_SERVICE_USAGE.md). For service
ownership and trust boundaries, see [`ARCHITECTURE.md`](../../ARCHITECTURE.md).
