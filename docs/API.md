# API

`payment-api` is the only process. It starts configured Bitcoin/Ethereum
indexing workers, opens their RocksDB databases, composes concrete wallet
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
      "database": "./data/bitcoin",
      "network": "Regtest",
      "genesis_hash": "<canonical-bitcoin-genesis-hash>",
      "rpc": {
        "endpoints": ["http://127.0.0.1:18443"],
        "headers": [],
        "timeout_seconds": 15,
        "max_response_bytes": 67108864
      },
      "bootstrap_height": 0,
      "confirmation_depth": 1,
      "reorg_retention": 100,
      "poll_millis": 1000,
      "batch_size": 256
    },
    "ethereum": {
      "database": "./data/ethereum",
      "network": "local",
      "chain_id": 31337,
      "genesis_hash": "0x<64-hex-digits>",
      "rpc": {
        "endpoints": ["http://127.0.0.1:8545"],
        "headers": [],
        "timeout_seconds": 15,
        "max_response_bytes": 67108864
      },
      "bootstrap_height": 0,
      "confirmation_depth": 1,
      "reorg_retention": 100,
      "poll_millis": 1000,
      "batch_size": 256
    }
  }
}
```

At least one chain is required. Each database path must have one process owner.
When `tls_terminated_upstream` is false, the server accepts only a loopback
bind. Otherwise TLS must be terminated by trusted upstream infrastructure.

RPC endpoints are ordered. Generic transport retries retryable failures and
may advance to the next endpoint. Headers are configuration data and must not
be logged. Each chain shares one connected client between its indexing source
and wallet fee/account/transaction capabilities. Ethereum also accepts an
optional `limits` object for maximum input bytes, gas margin/limit, per-gas
fees, priority fee, and total fee; omitting it uses validated SDK defaults.

## Authentication

Every wallet route requires:

```text
Authorization: Bearer <value from PAYMENT_API_TOKEN>
```

`GET /health/live` and `GET /health/ready` return `204 No Content` and are
public. Liveness means the process is running. Before binding the listener,
`Runtime` waits until every
configured embedded index reports `SyncPhase::Ready` and has persisted a
canonical checkpoint. This guarantees a newly created wallet can derive a
watch birthday immediately. A worker exit fails startup; a fatal worker error
after startup terminates the runtime rather than leave a silently stale API.

`GET /openapi.json` is also public and returns the generated OpenAPI 3 contract.
The wallet and transaction resources are bearer-protected. Utoipa annotations
and the Axum resource routers generate the contract from the same handlers,
avoiding a manually duplicated route specification.

## Create a wallet

```http
POST /v1/wallets
Content-Type: application/json

{"chain":"bitcoin"}
```

Response (`201 Created`):

```json
{
  "id": "019...",
  "chain": "bitcoin",
  "network": "regtest",
  "address": "bcrt1..."
}
```

The chain must have been configured at startup. Network is selected by that
configuration, not accepted from the request. Before returning `201`, the API
durably registers an address watch whose birthday is immediately after the
current checkpoint (or the configured bootstrap beginning when no checkpoint
exists).

The current key and wallet catalog are intentionally in memory. Restarting the
process loses generated private keys and wallet IDs, although indexing watches,
checkpoints, and history remain in RocksDB. This is development behavior, not
production custody or durable wallet management.

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

## Read indexed transactions

```http
GET /v1/wallets/{id}/transactions?limit=100&cursor=<opaque>
```

```json
{
  "transactions": [],
  "next_cursor": null
}
```

`GET` and `POST` deliberately share the wallet transaction resource: `GET`
reads indexed transactions and `POST` submits one new transaction. `limit`
defaults to 100 and must be between 1 and 1000. Treat `cursor` as an opaque value
and return it unchanged on the next request. Transactions contain the wallet SDK's complete
transaction representation: scoped identity, revision, status, all movements,
optional fee, and observation ordering values. Bitcoin inputs and outputs
remain separate movements.

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

- one request targets one chain; mixed-chain wallet IDs are rejected before an
  external effect;
- Bitcoin may combine several source wallets into one transaction, reading all
  sources at one checkpoint, preserving distinct per-source change, signing
  each input with its owner, and creating one requested output per transfer;
  and
- Ethereum builds separate transactions and broadcasts them in input order,
  using consecutive nonces for repeated transfers from the same wallet and
  reporting any accepted prefix if a later broadcast fails.

The one-process acceptance suite proves a two-transfer Bitcoin batch produces
one transaction/ID and a two-transfer Ethereum batch produces two IDs in input
order, with both results later visible through indexing. Focused public-API
evidence is still required for multi-source Bitcoin change/signatures and an
Ethereum failure after a submitted prefix.

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
| `404` | chain is not configured or wallet ID is unknown |
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
