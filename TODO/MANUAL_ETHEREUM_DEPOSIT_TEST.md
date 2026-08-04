# Manual Ethereum deposit-observation test

Status: Draft — pending review.

This runbook manually exercises the Ethereum path that is runnable today:

1. generate a disposable Ethereum deposit address;
2. register the address directly with the Indexer Service (IX);
3. send local Anvil ETH to the address; and
4. read the resulting `Included` and `Confirmed` observation revisions.

This is an IX-level deposit-observation test, not the complete production
Payment Service (PS) deposit flow. The current application does not expose a
public PS deposit-creation endpoint or a Wallet Service address endpoint.
Following this runbook therefore bypasses PS persistence of
`AwaitingWatch -> Active` and does not validate business classification, user
accounting, crediting, collection, or recovery of deposited funds.

> **Never send real funds to the generated address.** The example uses
> `LocalSigner::ephemeral_for_testing()`. Its private key remains only in memory
> and is discarded when the example process exits, so the address cannot be
> swept afterward. Use only disposable ETH on the local Anvil chain described
> below.

## Prerequisites

Run every command from the repository root. The procedure requires:

- Rust and Cargo;
- Foundry's `anvil` and `cast` commands;
- `curl`; and
- `jq`.

Use three terminals and leave the Anvil and Indexer processes running until the
test is complete.

## 1. Start Anvil

In terminal 1, start a fresh local chain with Ethereum chain ID `31337`:

```bash
anvil \
  --host 127.0.0.1 \
  --port 8545 \
  --chain-id 31337 \
  --quiet
```

The `--quiet` flag prevents Anvil from printing its pre-funded test private
keys. The later transfer uses one of Anvil's unlocked accounts and does not
require exposing a private key.

## 2. Start the Ethereum Indexer

In terminal 2, read the local chain's genesis hash and allocate an isolated IX
database path:

```bash
export MANUAL_RPC_URL=http://127.0.0.1:8545

export MANUAL_GENESIS_HASH="$(
  cast rpc eth_getBlockByNumber 0x0 false \
    --rpc-url "$MANUAL_RPC_URL" |
    jq -r '.hash'
)"

export MANUAL_IX_DB="$(mktemp -d /tmp/payment-sdk-ix.XXXXXX)/db"
```

Start the Indexer with the Ethereum v1 confirmation and reorg-retention policy:

```bash
cargo run --locked -p indexer-worker -- serve \
  --database-path "$MANUAL_IX_DB" \
  --network anvil \
  --bootstrap-height 0 \
  --confirmation-depth 12 \
  --reorg-retention 50 \
  --expected-chain-id 31337 \
  --expected-genesis-hash "$MANUAL_GENESIS_HASH" \
  --rpc-http-url "$MANUAL_RPC_URL" \
  --http-bind 127.0.0.1:8080 \
  --metrics-bind 127.0.0.1:9090 \
  --poll-seconds 1
```

Leave this command running. IX uses HTTP reconciliation as its canonical source
and polls Anvil once per second for this manual test.

## 3. Check readiness and capture the checkpoint

In terminal 3, configure the two local service origins:

```bash
export MANUAL_RPC_URL=http://127.0.0.1:8545
export MANUAL_IX_URL=http://127.0.0.1:8080
```

Check readiness:

```bash
curl -fsS "$MANUAL_IX_URL/health/ready" | jq
```

Expected response:

```json
{
  "status": "ready"
}
```

If the request fails while IX is starting, wait briefly and retry it. Do not
register the watch before readiness succeeds.

Capture the current canonical checkpoint as the address birthday:

```bash
export MANUAL_START_HEIGHT="$(
  curl -fsS \
    "$MANUAL_IX_URL/v1/scopes/ethereum/anvil/status" |
    jq -r '.checkpoint.height'
)"

printf 'start_height=%s\n' "$MANUAL_START_HEIGHT"
```

On a newly started Anvil chain, the expected start height is `0`.

## 4. Generate the disposable ETH address

Run the repository's offline Ethereum wallet example and capture its address:

```bash
export MANUAL_WALLET_OUTPUT="$(
  cargo run --locked --quiet \
    -p chain-ethereum \
    --example ethereum_test_wallet
)"

printf '%s\n' "$MANUAL_WALLET_OUTPUT"

export MANUAL_DEPOSIT_ADDRESS="$(
  printf '%s\n' "$MANUAL_WALLET_OUTPUT" |
    sed -n 's/^address: //p'
)"

printf 'deposit_address=%s\n' "$MANUAL_DEPOSIT_ADDRESS"
```

The output should contain an `0x`-prefixed address, an opaque key locator, a
public key, and the warning that the private key is lost when the process exits.

## 5. Register and verify the address watch

Construct the IX watch request with the generated address and captured
checkpoint:

```bash
export MANUAL_WATCH_BODY="$(
  jq -nc \
    --arg address "$MANUAL_DEPOSIT_ADDRESS" \
    --arg start "$MANUAL_START_HEIGHT" \
    '{
      selector: {
        type: "address",
        value: $address
      },
      start_height: $start,
      idempotency_key: "manual-eth-deposit-001"
    }'
)"
```

Register the watch and retain its ID:

```bash
export MANUAL_WATCH_RESPONSE="$(
  curl -fsS \
    -H 'content-type: application/json' \
    --data "$MANUAL_WATCH_BODY" \
    "$MANUAL_IX_URL/v1/scopes/ethereum/anvil/watches"
)"

printf '%s\n' "$MANUAL_WATCH_RESPONSE" | jq

export MANUAL_WATCH_ID="$(
  printf '%s\n' "$MANUAL_WATCH_RESPONSE" |
    jq -r '.id'
)"
```

Verify that the response contains:

- a non-empty `id`;
- scope `ethereum/anvil`;
- the generated address;
- the captured start height;
- `inactive_from: null`; and
- confirmation depth `12`.

Repeat the exact request to verify watch-registration idempotency:

```bash
curl -fsS \
  -H 'content-type: application/json' \
  --data "$MANUAL_WATCH_BODY" \
  "$MANUAL_IX_URL/v1/scopes/ethereum/anvil/watches" |
  jq -r '.id'
```

The returned ID must equal `MANUAL_WATCH_ID`. Reusing the same idempotency key
with a different address or start height is a conflict and is not an idempotent
retry.

## 6. Send local ETH to the watched address

Read the first unlocked Anvil account. This returns only its public address:

```bash
export MANUAL_SENDER="$(
  cast rpc eth_accounts \
    --rpc-url "$MANUAL_RPC_URL" |
    jq -r '.[0]'
)"
```

Send `0.01` disposable Anvil ETH and capture the mined transaction hash:

```bash
export MANUAL_TX_HASH="$(
  cast send "$MANUAL_DEPOSIT_ADDRESS" \
    --value 0.01ether \
    --from "$MANUAL_SENDER" \
    --unlocked \
    --rpc-url "$MANUAL_RPC_URL" \
    --json |
    jq -r '.transactionHash'
)"

printf 'transaction_hash=%s\n' "$MANUAL_TX_HASH"
```

`--unlocked` asks Anvil to execute `eth_sendTransaction`; no private key is
passed to `cast` or printed by this runbook.

## 7. Catch the Included observation

Allow one or two polling cycles, then query IX's replayable event feed:

```bash
sleep 2

curl -fsS \
  "$MANUAL_IX_URL/v1/events?after_cursor=0&limit=100" |
  jq \
    --arg watch_id "$MANUAL_WATCH_ID" \
    --arg tx_hash "$MANUAL_TX_HASH" \
    '
      .events[]
      | select((.watch_ids | index($watch_id)) != null)
      | select(.transaction.transaction_id == $tx_hash)
    '
```

The first matching revision must contain:

- `watch_ids` containing `MANUAL_WATCH_ID`;
- `transaction.transaction_id` equal to `MANUAL_TX_HASH`;
- `transaction.status.kind` equal to `included`;
- an `asset: "native"` movement;
- the generated address in the movement's `to` field; and
- the transferred amount encoded as a 32-byte hexadecimal atomic value.

IX deliberately reports a chain observation/fact rather than a business-level
`DepositReceived` event. Deposit classification belongs to PS and is outside
this direct-IX test.

The same transaction can be queried through the address index:

```bash
curl -fsS \
  "$MANUAL_IX_URL/v1/scopes/ethereum/anvil/addresses/$MANUAL_DEPOSIT_ADDRESS/transactions" |
  jq
```

## 8. Verify depth-12 confirmation

The transfer is included at depth 1. Mine eleven empty blocks to reach the
configured confirmation depth of 12:

```bash
cast rpc anvil_mine 0xb \
  --rpc-url "$MANUAL_RPC_URL"
```

After IX processes the blocks, query for the matching `confirmed` revision:

```bash
sleep 2

curl -fsS \
  "$MANUAL_IX_URL/v1/events?after_cursor=0&limit=100" |
  jq \
    --arg watch_id "$MANUAL_WATCH_ID" \
    --arg tx_hash "$MANUAL_TX_HASH" \
    '
      .events[]
      | select((.watch_ids | index($watch_id)) != null)
      | select(.transaction.transaction_id == $tx_hash)
      | select(.transaction.status.kind == "confirmed")
    '
```

If no row is returned yet, allow another polling cycle and repeat the query.
The final revision must include this depth proof:

```json
{
  "kind": "depth",
  "required": "12",
  "observed": "12"
}
```

The feed will also contain immutable intermediate `Included` revisions as the
observed depth advances. Consumers must use event IDs/revisions or feed cursors
for idempotency rather than deduplicating only by transaction hash and status.

## 9. Optionally mirror IX events into PS storage

This optional step proves that the current PS maintenance runtime can durably
copy the IX feed. It still does not create a deposit, classify the movement,
credit a user, or advance a business projection.

Create a PS-owned database path that is physically separate from the IX path:

```bash
export MANUAL_PS_DB="$(mktemp -d /tmp/payment-sdk-ps.XXXXXX)/db"
```

Ingest the current IX event feed:

```bash
cargo run --locked -p payment-api -- ingest-events \
  --database-path "$MANUAL_PS_DB" \
  --indexer-url "$MANUAL_IX_URL" \
  --network anvil
```

The output should report at least one appended event and an advanced ingestion
cursor. Inspect the independent projection backlog:

```bash
cargo run --locked -p payment-api -- projection-status \
  --database-path "$MANUAL_PS_DB"
```

The output must include `classification_configured=false`. That value is the
explicit boundary between the implemented durable event mirror and the pending
PS business-classification composition.

## Success criteria

The manual test passes when all of the following are true:

- IX becomes ready and reports the expected Anvil scope.
- Address generation returns a valid Ethereum address without exposing a
  private key.
- Repeating an identical watch request returns the same watch ID.
- The mined transfer appears in both the IX event feed and address query.
- The first observation is `Included` and references the registered watch.
- After eleven additional blocks, a later revision is `Confirmed` with required
  and observed depth `12`.
- If the optional PS step is run, the ingestion cursor advances while business
  classification remains explicitly disabled.

## Shutdown and retained test data

Stop the Indexer in terminal 2 and Anvil in terminal 1 with `Ctrl-C`. The
isolated IX and PS databases were created below `/tmp`; retain them temporarily
if they are useful for review, or remove those specific test directories after
the processes have stopped.
