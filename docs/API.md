# API

`payment-api` is the only process. It starts configured Bitcoin/Ethereum
indexing workers, opens their redb files, composes concrete wallet
providers, and serves one authenticated wallet API.

## Run

```bash
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-api -- ./config.json
```

The configuration file names the environment variable containing the inbound
bearer token. The token itself must not be stored in JSON.

Minimal shape with both chains:

```json
{
  "bind": "127.0.0.1:8080",
  "bearer_token_env": "PAYMENT_API_TOKEN",
  "tls_terminated_upstream": false,
  "indexes": {
    "bitcoin": {
      "database": "/var/lib/payment-sdk/bitcoin.redb",
      "network": "Regtest",
      "genesis_hash": "<canonical-bitcoin-genesis-hash>",
      "rpc": {
        "endpoints": ["http://127.0.0.1:18443"],
        "headers": [],
        "timeout_seconds": 15,
        "max_response_bytes": 67108864
      },
      "confirmation_depth": 1,
      "reorg_retention": 100,
      "poll_millis": 1000,
      "batch_size": 256
    },
    "ethereum": {
      "database": "/var/lib/payment-sdk/ethereum.redb",
      "network": "local",
      "chain_id": 31337,
      "genesis_hash": "0x<64-hex-digits>",
      "rpc": {
        "endpoints": ["http://127.0.0.1:8545"],
        "headers": [],
        "timeout_seconds": 15,
        "max_response_bytes": 67108864
      },
      "usdc": {
        "contract": "0x<40-hex-digits>"
      },
      "confirmation_depth": 1,
      "reorg_retention": 100,
      "poll_millis": 1000,
      "batch_size": 256
    }
  },
  "wallets": [
    {
      "id": "treasury-usdc",
      "asset": "usdc",
      "secret_env": "TREASURY_USDC_SECRET",
      "start_height": 1
    }
  ]
}
```

At least one chain is required. Each `database` value is an absolute path to
one redb file. Its parent directory must already exist, the path must not be an
existing directory, and each file must have one process owner. A RocksDB
directory is not a valid redb file and is rejected rather than converted.
When `tls_terminated_upstream` is false, the server accepts only a loopback
bind. Otherwise TLS must be terminated by trusted upstream infrastructure.

The embedded database uses immediate durable commits and a bounded 128 MiB
cache per file. Backups are cold copies: stop the process, wait for shutdown to
close the database, copy the single `.redb` file, and verify that the copy opens
before relying on it. Do not copy an open file.

Replacing an earlier RocksDB deployment requires fresh redb files and a rescan
from the configured wallet birthdays. Keep the old binary, configuration, and
RocksDB directories untouched until checkpoints, balances, history, and live
Bitcoin outputs have been compared. The configured RPC providers must retain
historical blocks and receipts back to every birthday; a pruned provider can
make that rescan incomplete.

For the cutover, record each chain's ready checkpoint, catch-up throughput,
commit latency, crash-reopen time, resident memory, cache setting, and database
file size. Compare complete paginated history—not only the first page—and all
live Bitcoin outputs before switching traffic. Rollback means stopping the new
process and restarting the old binary with its unchanged configuration and
RocksDB directories; there is no in-place conversion or dual-write mode.

RPC endpoints are ordered. Generic transport retries retryable failures and
may advance to the next endpoint. Headers are configuration data and must not
be logged. Each chain shares one connected client between its indexing source
and wallet fee/account/transaction capabilities. Ethereum also accepts an
optional `limits` object for maximum input bytes, gas margin/limit, per-gas
fees, priority fee, and total fee; omitting it uses validated SDK defaults.
The optional `usdc` object allowlists one exact contract for that Ethereum
network. Startup requires nonempty contract code and verifies the expected six
decimals. Until RPC endpoint identity is admitted and validated independently,
enabling `usdc` requires exactly one Ethereum RPC endpoint so chain identity,
canonical block selection, and contract probes cannot span different nodes.
Native-only Ethereum configuration may still use ordered endpoint failover.
HTTP callers never choose a token contract or decimals. Omitting `usdc` leaves
native ETH enabled without a USDC wallet family.

## Authentication

Every wallet route requires:

```text
Authorization: Bearer <value from PAYMENT_API_TOKEN>
```

`GET /health/live` and `GET /health/ready` return `204 No Content` and are
public. Liveness means the process is running. Before binding the listener, the
composition root waits until every configured synchronizer reports that it has
caught up with its node. Readiness is runtime state; it is not persisted in
redb. This guarantees a newly created wallet can derive an address birthday
immediately. A synchronizer exit fails startup; a fatal error
after startup terminates the runtime rather than leave a silently stale API.

`GET /openapi.json` is also public and returns the generated OpenAPI 3 contract.
The wallet and transaction resources are bearer-protected. Utoipa annotations
and the Axum resource routers generate the contract from the same handlers,
avoiding a manually duplicated route specification.

## Create a wallet

```http
POST /v1/wallets
Content-Type: application/json

{"asset":"btc"}
```

Response (`201 Created`):

```json
{
  "id": "019...",
  "asset": "btc",
  "chain": "bitcoin",
  "network": "regtest",
  "address": "bcrt1..."
}
```

The asset must have been configured at startup. Accepted values are `btc`,
`eth`, and `usdc`; `usdc` is available only when its contract is configured.
Network and token contract are selected by startup configuration, not accepted
from the request. Each generation selects exactly one payment asset, and ETH
and USDC generations create independent keys and addresses. Before returning
`201`, the API adds the address to the synchronizer's in-memory filters. Its
birthday is immediately after the current checkpoint, or the configured
beginning when no checkpoint exists.

Startup imports also enforce one asset per Ethereum address. The same imported
EOA cannot be registered once as `eth` and again as `usdc`; a USDC address may
still receive native ETH externally for gas, but that ETH is not exposed as a
second wallet asset.

The current key and wallet catalog are intentionally in memory. Restarting the
process loses generated private keys, wallet IDs, and their indexing filters.
Canonical checkpoints and indexed history remain in redb. This is
development behavior, not production custody or durable wallet management.

## Read wallet metadata

```http
GET /v1/wallets/{id}
```

Returns the same public wallet summary. Private key material is never returned.

## Read balance

```http
GET /v1/wallets/{id}/balance
```

```json
{
  "amount": "12.5",
  "observed_height": 42
}
```

The amount is an exact decimal string in the wallet asset's display units.
`observed_height` is null before a canonical checkpoint is available.
For a USDC wallet this is only its USDC balance. Native ETH required for gas is
validated internally when sending and is not exposed as a second wallet asset.

## Read indexed transactions

```http
GET /v1/wallets/{id}/transactions?limit=100&cursor=<opaque>
```

```json
{
  "checkpoint": {
    "height": 42,
    "hash": "<hex>",
    "parent_hash": "<hex>"
  },
  "transactions": [],
  "next_cursor": null
}
```

`GET` and `POST` deliberately share the wallet transaction resource: `GET`
reads indexed transactions and `POST` submits one new transaction. `limit`
defaults to 100 and must be between 1 and 1000. Treat `cursor` as an opaque value
and return it unchanged on the next request. The cursor binds pagination to the
response checkpoint; if canonical history changes, the API returns a conflict
and the caller restarts from the first page. Transactions contain scoped
identity, canonical inclusion/confirmation status, all movements, and an
optional fee. Bitcoin inputs and outputs remain separate movements.

Ethereum pages contain movements for the wallet's selected asset. A USDC page
keeps the transaction's attributable ETH network fee but omits unrelated ETH
or token movements. Filtering preserves the underlying checkpoint and cursor,
so a page may contain fewer than `limit` items, or be empty while still
returning `next_cursor`.

## Send one transfer

```http
POST /v1/wallets/{id}/transactions
Content-Type: application/json

{
  "destination": {
    "encoding": "bech32",
    "text": "<chain address>"
  },
  "amount": "1.25"
}
```

After the concrete wallet validates the address and exact positive decimal,
builds and signs its chain-native transaction, and a node accepts it for
submission, the API returns `202 Accepted`:

```json
{"transaction_id":"<canonical chain transaction id>"}
```

This response means submitted, not confirmed. Read indexed history to observe
inclusion, confirmation, replacement, failure, or reorg. The endpoint has no
durable payment operation or idempotency state; callers must not interpret an
ambiguous transport outcome as proof that no broadcast occurred.

## Send several transfers

The in-progress batch surface is:

```http
POST /v1/transactions
Content-Type: application/json

{
  "transfers": [
    {
      "wallet_id": "019...",
      "destination": {"encoding": "hex", "text": "<address>"},
      "amount": "2.5"
    }
  ]
}
```

Its success response is `202 Accepted` with the submitted chain-native IDs:

```json
{"transaction_ids":["<transaction id>"]}
```

Required semantics are:

- one request targets one exact wallet asset; mixed-family wallet IDs,
  including ETH and USDC on the same chain, are rejected before an external
  effect;
- Bitcoin may combine several source wallets into one transaction, reading all
  sources at one checkpoint, preserving distinct per-source change, signing
  each input with its owner, and creating one requested output per transfer;
  and
- Ethereum builds separate transactions and broadcasts them in input order,
  reporting any accepted prefix if a later broadcast fails.

The one-process acceptance suite proves a two-transfer Bitcoin batch produces
one transaction/ID and a two-transfer Ethereum batch produces two IDs in input
order, with both results later visible through indexing. It also proves
multi-source Bitcoin signatures, an Ethereum failure after a submitted prefix,
and zero-broadcast rejection for mixed ETH/USDC families. Consecutive nonce
reservation, whole-batch Ethereum preflight, and ambiguous-submission
reconciliation remain the next transaction-safety phase.

If a sequential batch fails after a prefix was accepted, the error body keeps
the ordinary `message` and adds the accepted `transaction_ids` plus the
zero-based `failed_index` from the original request:

```json
{
  "message": "node rejected transaction",
  "transaction_ids": ["<accepted transaction id>"],
  "failed_index": 1
}
```

## Errors

Errors return JSON with one `message` field. Current mappings are:

| Status | Meaning |
|---|---|
| `400` | malformed request or cursor |
| `404` | asset is not configured or wallet ID is unknown |
| `409` | duplicate/conflicting composition state |
| `503` | wallet or indexing operation unavailable |
| `500` | an internal result could not be encoded |

Batch transaction failures use `422`; their body may also include accepted
transaction IDs and the failed input index as shown above.

## Current scope

The public routes cover wallet generation, metadata, balance, history, ordinary
sending, and chain-native batch sending as described above.
There are no deposit, payment-state, accounting, collection, indexer-service,
or wallet-service routes.
