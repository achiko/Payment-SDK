# Manual Bitcoin regtest Payment Service happy path

Status: manually exercised on 2026-08-11 with Bitcoin Core 31.1 in
`global_trusted` authentication mode.

The repository-owned
[`automated system-acceptance suite`](../../tests/bitcoin-regtest-acceptance/README.md)
is the primary repeatable check and passed all eight strict/global-trusted
executions on the same date. This short guide remains a diagnostic walkthrough.

This is the short, manual walkthrough for the successful path:

```text
Bitcoin Core regtest
  -> Indexer Service (IX)
  -> ephemeral custody
  -> Wallet Service (WS)
  -> Payment Service (PS)
  -> deposit payment
  -> confirmed collection to the policy master address
```

Run one command at a time and verify each checkpoint before continuing.
Generated addresses, UUIDs, transaction IDs, block hashes, timestamps, and job
attempt counts will differ from the reference run.

For strict bearer authentication, P2TR, restart/replay, controlled reorg, and
re-inclusion testing, use the extended [Core 31 regtest acceptance
guide](./README.md). Passing this happy path does not prove those scenarios.

## Safety boundary

- Use **only Bitcoin regtest**. Every address must begin with `bcrt1`.
- Keep Core and all HTTP listeners on loopback.
- Custody is in-memory and ephemeral. Do not restart custody after creating a
  deposit address; its private key would be lost.
- Never send real bitcoin to any address in this guide.
- Core cookie authentication remains required even though service
  authentication is globally trusted.
- `STRICT_AUTHENTICATION_MODE=false` gives every caller that can reach a
  service full application authority. This is suitable only for this
  disposable loopback walkthrough.

## Prerequisites and terminals

The commands assume `zsh` or `bash`, `curl`, `jq`, `base64`, Rust/Cargo, and
Bitcoin Core 31.1. They do not assume any absolute repository or Bitcoin Core
installation path.

Use six terminals and leave each foreground process running:

1. Bitcoin Core;
2. IX;
3. custody;
4. WS;
5. PS; and
6. manual control commands.

Open each Rust-service terminal at the repository root. If `bitcoind` and
`bitcoin-cli` are not on `PATH`, set `BITCOIND` and `BITCOIN_CLI` to their
locations in your private shell environment. Do not commit those paths.

In the control terminal:

```bash
export BITCOIND="${BITCOIND:-bitcoind}"
export BITCOIN_CLI="${BITCOIN_CLI:-bitcoin-cli}"
```

## 1. Start Bitcoin Core in the foreground

In the Bitcoin Core terminal, set `BITCOIND` as above and run:

```bash
"$BITCOIND" \
  -regtest \
  -txindex=1 \
  -prune=0 \
  -fallbackfee=0.0001 \
  -printtoconsole=1
```

This keeps Core in the foreground and prints its logs. `-fallbackfee` is a
wallet fallback; it does not seed `estimatesmartfee`.

This short guide intentionally uses Core's default regtest datadir, matching
the reference run, and never deletes it. Use the extended guide when a fresh,
isolated Core datadir is part of the acceptance criteria.

In the control terminal, verify the configured binaries:

```bash
"$BITCOIND" -nosettings --version
"$BITCOIN_CLI" --version
```

Verify the live node:

```bash
"$BITCOIN_CLI" -regtest getblockchaininfo | jq
"$BITCOIN_CLI" -regtest getindexinfo txindex | jq
```

Required checkpoint:

- `chain` is `regtest`;
- `pruned` is `false`;
- `initialblockdownload` is `false`;
- block and header heights match; and
- `txindex.synced` is `true`.

The conventional regtest genesis hash used by IX and WS is:

```text
0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206
```

Verify it rather than trusting the constant:

```bash
"$BITCOIN_CLI" -regtest getblockhash 0
```

## 2. Start the Indexer Service

Core's RPC cookie is node authentication. It is independent of the
repo-wide service authentication mode. The following expression reads it into
the IX process environment without printing the cookie. First set the cookie
path privately for the active Core datadir:

```bash
export BITCOIN_COOKIE="<path-to-active-regtest-cookie>"
test -r "$BITCOIN_COOKIE"
```

Then start IX:

```bash
IX_RPC_HEADERS="authorization=Basic $(base64 < "$BITCOIN_COOKIE" | tr -d '\r\n')" \
STRICT_AUTHENTICATION_MODE=false \
IX_DATABASE_PATH=./tmp/bitcoin-indexer-regtest-live \
IX_NETWORK=regtest \
IX_BOOTSTRAP_HEIGHT=0 \
IX_CONFIRMATION_DEPTH=2 \
IX_REORG_RETENTION=20 \
IX_EXPECTED_GENESIS_HASH=0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206 \
IX_RPC_HTTP_URL=http://127.0.0.1:18443 \
IX_HTTP_BIND=127.0.0.1:8080 \
IX_METRICS_BIND=127.0.0.1:9090 \
IX_POLL_SECONDS=1 \
IX_READY_MAX_LAG=0 \
cargo run --locked -p indexer-worker -- bitcoin serve
```

Use a new IX database path when starting a different regtest history or
changing its confirmation/retention policy.

Verify public readiness and the canonical scope:

```bash
curl --fail --silent --show-error \
  'http://127.0.0.1:8080/health/ready' | jq

curl --fail --silent --show-error \
  'http://127.0.0.1:8080/v1/scopes/bitcoin/regtest/status' | jq
```

Expected readiness:

```json
{
  "status": "ready",
  "authentication_mode": "global_trusted"
}
```

The scope status must report `phase: "ready"`, confirmation depth `"2"`, and
a checkpoint matching Core's current tip.

## 3. Start ephemeral custody

In the custody terminal:

```bash
STRICT_AUTHENTICATION_MODE=false \
cargo run --locked -p custody-worker -- serve
```

Verify readiness:

```bash
curl --fail --silent --show-error \
  'http://127.0.0.1:8181/health/ready' | jq
```

Keep this process alive for the entire run. Its generated keys exist only in
memory.

## 4. Start the Bitcoin Wallet Service

In the WS terminal, set `BITCOIN_COOKIE` to the same private cookie path, then
start WS:

```bash
export BITCOIN_COOKIE="<path-to-active-regtest-cookie>"

WS_BITCOIN_CORE_RPC_AUTHORIZATION="Basic $(base64 < "$BITCOIN_COOKIE" | tr -d '\r\n')" \
STRICT_AUTHENTICATION_MODE=false \
WS_BITCOIN_NETWORK=regtest \
WS_BITCOIN_EXPECTED_GENESIS_HASH=0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206 \
WS_BITCOIN_CORE_RPC_URL=http://127.0.0.1:18443 \
WS_BITCOIN_IX_URL=http://127.0.0.1:8080 \
WS_BITCOIN_MINIMUM_CONFIRMATIONS=1 \
WS_BITCOIN_FEE_TARGET_BLOCKS=6 \
WS_BITCOIN_MAX_SATOSHIS_PER_KVB=100000 \
WS_CUSTODY_URL=http://127.0.0.1:8181 \
WS_CUSTODY_AUTHENTICATION_POLICY=repository_mode_matched \
WS_HTTP_BIND=127.0.0.1:8082 \
WS_METRICS_BIND=127.0.0.1:9092 \
cargo run --locked -p wallet-worker -- bitcoin serve
```

Verify readiness:

```bash
curl --fail --silent --show-error \
  'http://127.0.0.1:8082/health/ready' | jq
```

Expected `authentication_mode` is `global_trusted`. A warning that bearer
credentials are ignored is expected in this mode.

## 5. Create and mature the miner wallet

Check loaded wallets:

```bash
"$BITCOIN_CLI" -regtest listwallets
```

For a new wallet:

```bash
"$BITCOIN_CLI" -regtest createwallet "miner"
```

If Core reports that the wallet database already exists but `listwallets`
does not show it, load it instead:

```bash
"$BITCOIN_CLI" -regtest loadwallet "miner"
```

Create a mining address:

```bash
"$BITCOIN_CLI" -regtest -rpcwallet=miner \
  getnewaddress "payment-sdk-mining" bech32
```

Copy the returned `bcrt1...` value and use it as `<MINING_ADDRESS>` below:

```bash
"$BITCOIN_CLI" -regtest generatetoaddress 101 "<MINING_ADDRESS>"
```

Verify the wallet:

```bash
"$BITCOIN_CLI" -regtest -rpcwallet=miner getbalances | jq
```

On a fresh chain, one `50 BTC` coinbase output should now be `trusted`; the
newer coinbase outputs remain `immature`. Coinbase rewards require 100 blocks
of maturity before they can be spent.

## 6. Warm the fee estimator

A fresh regtest chain has no fee market history, so this may initially fail:

```bash
"$BITCOIN_CLI" -regtest estimatesmartfee 6 conservative | jq
```

Create one miner-wallet address for wallet-to-itself fee samples:

```bash
"$BITCOIN_CLI" -regtest -rpcwallet=miner \
  getnewaddress "fee-warmup" bech32
```

Copy it as `<FEE_WARMUP_ADDRESS>`. Submit twelve transactions at exactly
`2 sat/vB`:

```bash
for i in {1..12}; do
  "$BITCOIN_CLI" -named -regtest -rpcwallet=miner sendtoaddress \
    address="<FEE_WARMUP_ADDRESS>" \
    amount=0.001 \
    fee_rate=2
done
```

Mine them together:

```bash
"$BITCOIN_CLI" -regtest generatetoaddress 1 "<MINING_ADDRESS>"
```

Check the estimator again:

```bash
"$BITCOIN_CLI" -regtest estimatesmartfee 6 conservative | jq
```

If it still reports `Insufficient data or no feerate found`, repeat the same
twelve-transaction batch and mine one more block. In the reference run, three
batches across three blocks produced:

```json
{
  "feerate": 0.00002000,
  "blocks": 2
}
```

`0.00002000 BTC/kvB` equals `2000 sat/kvB`, or `2 sat/vB`. Core 31.1 does not
provide the old `settxfee` RPC used by some earlier test instructions; the
named `sendtoaddress fee_rate=2` argument is the correct deterministic warm-up
control here.

## 7. Create the Payment Service master destination and policy

Create a dedicated regtest master destination:

```bash
"$BITCOIN_CLI" -regtest -rpcwallet=miner \
  getnewaddress "payment-sdk-master" bech32
```

Copy the returned address as `<MASTER_ADDRESS>`. It must begin with `bcrt1`.

Create `./tmp/bitcoin-ps-policy.json` with these exact fields, replacing only
`<MASTER_ADDRESS>`:

```bash
mkdir -p ./tmp
```

```json
{
  "version": 1,
  "scope": {
    "chain": "bitcoin",
    "network": "regtest"
  },
  "deposit_address_kind": "p2wpkh",
  "deposit_ttl_seconds": 3600,
  "master_destination": "<MASTER_ADDRESS>",
  "minimum_collection_satoshis": "10000",
  "minimum_spend_confirmations": 1,
  "requested_satoshis_per_kvb": "1000",
  "maximum_satoshis_per_kvb": "5000",
  "maximum_absolute_fee_satoshis": "50000",
  "maximum_deposits": 20,
  "maximum_inputs": 200
}
```

This is PS business and safety policy, not Bitcoin Core configuration. PS
binds the exact policy bytes and digest to its database, so do not edit the
file after starting PS against that database.

## 8. Start the Bitcoin Payment Service

In the PS terminal:

```bash
STRICT_AUTHENTICATION_MODE=false \
PS_DATABASE_PATH=./tmp/bitcoin-payment-regtest-live \
PS_POLICY_PATH=./tmp/bitcoin-ps-policy.json \
PS_INDEXER_URL=http://127.0.0.1:8080 \
PS_INDEXER_NETWORK=regtest \
PS_WALLET_URL=http://127.0.0.1:8082 \
PS_HTTP_BIND=127.0.0.1:8081 \
PS_METRICS_BIND=127.0.0.1:9091 \
cargo run --locked -p payment-api -- bitcoin serve
```

Verify readiness and dependency state:

```bash
curl --fail --silent --show-error \
  'http://127.0.0.1:8081/health/ready' | jq

curl --fail --silent --show-error \
  'http://127.0.0.1:8081/v1/admin/status' | jq
```

Required checkpoint:

- authentication mode is `global_trusted`;
- PS, IX, and WS are ready;
- scope is `bitcoin/regtest`; and
- event lag and job backlog are zero.

No administrator bearer is sent because global-trusted mode grants every
reachable caller administrator authority.

## 9. Create a PS-managed deposit

Create an exact `250000` satoshi deposit:

```bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header 'Content-Type: application/json' \
  --data '{
    "user_id":"manual-user-1",
    "scope":{"chain":"bitcoin","network":"regtest"},
    "asset":"native",
    "expected_amount":"250000"
  }' \
  'http://127.0.0.1:8081/v1/deposits' | jq
```

Copy the returned `deposit_id`. The response also includes a durable `job_id`
and a generated `idempotency_key` because the request omitted one in
global-trusted mode.

Read the deposit, replacing `<DEPOSIT_ID>`:

```bash
curl --fail-with-body --silent --show-error \
  'http://127.0.0.1:8081/v1/deposits/<DEPOSIT_ID>' | jq
```

Required checkpoint:

- state is `active`;
- payment progress is `unseen`;
- address begins with `bcrt1`; and
- the response has a birthday and expiration time.

PS returns the address only after it has persisted the deposit and IX has
durably acknowledged its watch.

## 10. Pay and confirm the deposit

This command creates and broadcasts a regtest transaction from the miner
wallet. Verify the destination is the PS deposit address before running it:

```bash
"$BITCOIN_CLI" -regtest -rpcwallet=miner \
  sendtoaddress "<DEPOSIT_ADDRESS>" 0.0025
```

Copy the returned funding transaction ID. Verify it is initially in the
mempool:

```bash
"$BITCOIN_CLI" -regtest getrawmempool | jq
```

Mine the first block:

```bash
"$BITCOIN_CLI" -regtest generatetoaddress 1 "<MINING_ADDRESS>"
```

Wait for IX and PS to poll, then read the balance:

```bash
curl --fail-with-body --silent --show-error \
  'http://127.0.0.1:8081/v1/deposits/<DEPOSIT_ID>/balances' | jq
```

With IX confirmation depth `2`, the first-block checkpoint is:

```json
{
  "received": "250000",
  "confirmed": "0",
  "balance": "250000",
  "collected": "0",
  "accounted": "0"
}
```

Mine the second block:

```bash
"$BITCOIN_CLI" -regtest generatetoaddress 1 "<MINING_ADDRESS>"
```

Read the balance again. It must now show `confirmed: "250000"` while
`balance` remains `"250000"`.

## 11. Create and observe the collection

This is the second funds-moving operation. PS will reserve the deposit UTXO,
ask WS/custody to sign, persist the exact signed bytes, and broadcast a
regtest-only sweep to the policy master address.

```bash
curl --fail-with-body --silent --show-error \
  --request POST \
  --header 'Content-Type: application/json' \
  --data '{"deposit_ids":["<DEPOSIT_ID>"]}' \
  'http://127.0.0.1:8081/v1/collections' | jq
```

Copy `job_id` and `collection_id`. HTTP `202 Accepted` proves only that the
durable command was queued.

Read the job:

```bash
curl --fail-with-body --silent --show-error \
  'http://127.0.0.1:8081/v1/jobs/<COLLECTION_JOB_ID>' | jq
```

While the transaction awaits IX confirmation, `waiting_retry` with
`dependency_not_ready` is expected and retryable.

Read the collection:

```bash
curl --fail-with-body --silent --show-error \
  'http://127.0.0.1:8081/v1/collections/<COLLECTION_ID>' | jq
```

Required broadcast checkpoint:

- collection state is `in_progress`;
- reservation state is `active`;
- one `sweep` leg is `broadcast`;
- the leg has a transaction ID and IX watch ID;
- gross debit is `250000`; and
- `master_credit + allocated_fee == 250000`.

Copy the collection transaction ID and inspect Core's mempool:

```bash
"$BITCOIN_CLI" -regtest getmempoolentry \
  "<COLLECTION_TRANSACTION_ID>" | jq
```

For the reference P2WPKH collection, Core reported `vsize: 110`, fee
`0.00000220 BTC` (`220 sats`), and therefore exactly `2 sat/vB`.
`bip125-replaceable: true` means the input sequence permits replacement before
confirmation. `unbroadcast: true` is normal on an isolated single-node
regtest: Core has not heard the transaction relayed back by a peer.

## 12. Confirm the collection

Mine the first block:

```bash
"$BITCOIN_CLI" -regtest generatetoaddress 1 "<MINING_ADDRESS>"
```

Read the collection again. At depth one it must remain `in_progress`, its leg
must remain `broadcast`, and its reservation must remain `active`.

Mine the second block:

```bash
"$BITCOIN_CLI" -regtest generatetoaddress 1 "<MINING_ADDRESS>"
```

Read the collection again:

```bash
curl --fail-with-body --silent --show-error \
  'http://127.0.0.1:8081/v1/collections/<COLLECTION_ID>' \
  | jq '{state, reservation, legs}'
```

Required confirmation checkpoint:

- collection state is `completed`;
- reservation state is `consumed` and names the collection transaction;
- the sweep leg is `confirmed`; and
- allocation amounts are unchanged from the broadcast state.

The job may remain `waiting_retry` until its already scheduled exponential
backoff expires. This does not undo the completed financial state. Poll it
after its `next_attempt_at`:

```bash
curl --fail-with-body --silent --show-error \
  'http://127.0.0.1:8081/v1/jobs/<COLLECTION_JOB_ID>' | jq
```

It must converge to `state: "succeeded"`, with `last_error` and
`next_attempt_at` both `null`.

## 13. Verify final PS accounting and the Core output

Read the deposit balance:

```bash
curl --fail-with-body --silent --show-error \
  'http://127.0.0.1:8081/v1/deposits/<DEPOSIT_ID>/balances' | jq
```

Expected final absolute snapshot:

```json
{
  "received": "250000",
  "confirmed": "250000",
  "balance": "0",
  "collected": "250000",
  "accounted": "0"
}
```

`collected` is the gross deposit debit. The master credit is smaller by the
separately recorded network fee. `accounted` stays zero because only an
explicit PS accounting command may change it.

Verify the confirmed Core transaction and destination:

```bash
"$BITCOIN_CLI" -regtest getrawtransaction \
  "<COLLECTION_TRANSACTION_ID>" true | jq '{
    txid,
    confirmations,
    vsize,
    outputs: [.vout[] | {
      index: .n,
      btc: .value,
      address: .scriptPubKey.address
    }]
  }'
```

The transaction must have at least two confirmations and exactly one output to
`<MASTER_ADDRESS>`. Convert BTC to satoshis before reconciling it with the PS
allocation; never compare money using floating-point application logic.

## 14. Reference result from the successful run

The 2026-08-11 reference run produced:

| Evidence | Result |
|---|---|
| Core | `31.1.0`, regtest, unpruned, synchronized txindex |
| Authentication | `global_trusted` on IX, custody, WS, and PS |
| IX policy | confirmation depth `2`, rollback retention `20` |
| Deposit | exact `250000 sats`, P2WPKH, Included then Confirmed |
| Funding txid | `e799225c9e793cfbe722399e79e222b5ea80d1d530cac9d7601580a9d65fc6c4` |
| Collection txid | `7f126b4ae5668cb884c973a8ce2fbec30389a4dc7702dc89697829abe8a77ee9` |
| Collection size | `110 vB` |
| Fee | `220 sats`, exactly `2 sat/vB` |
| Gross debit | `250000 sats` |
| Master credit | `249780 sats` |
| Final collection | `completed`; leg `confirmed`; job `succeeded` |
| Final deposit | balance `0`; collected `250000`; accounted `0` |

These IDs are evidence from one disposable regtest history, not constants for
future assertions.

## 15. Stop the run

Stop foreground processes with `Ctrl-C` in reverse dependency order:

1. PS;
2. WS;
3. custody;
4. IX; and
5. Bitcoin Core.

Stopping custody permanently destroys the disposable private keys from this
run. The IX and PS directories under `./tmp` remain until explicitly removed;
do not reuse them with a different regtest history, policy, or authentication
mode.

## Acceptance boundary

This guide proves one exact-payment/global-trusted happy path. It does not
claim acceptance for:

- strict bearer authentication;
- underpayment, overpayment, or partial-payment aggregation;
- P2TR deposits;
- idempotent request replay;
- process restart and durable replay;
- transaction conflict, RBF replacement, eviction, or CPFP;
- controlled reorg, UTXO restoration, and same-txid re-inclusion;
- custody durability; or
- any public network or production deployment.

Use the automated suite for repeatable acceptance and the extended manual guide
for step-by-step diagnosis of those cases.
