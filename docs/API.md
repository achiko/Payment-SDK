# API

`payment-api` is the only process. It starts configured Bitcoin/Ethereum
indexing workers, opens their redb files, composes concrete wallet
providers, and serves one authenticated wallet API.

The Public Transaction Semantics, Destination Account Acquisition, and Native
SOL Submission sections below describe accepted target contracts that are not
yet implemented. In particular, the current Rust source does not yet enforce
the shared 50-item maximum, reject transaction queries, project locally derived
ambiguous transaction IDs, or contain native SOL acquisition or submission.
`docs/FEATURE_VALIDATION.md` records those gaps; unmarked existing-runtime
descriptions remain current.

Native SOL Submission is Accepted and selects blockhash, fee, signing,
simulation, broadcast, exact-byte replay, and ambiguity behavior below. Solana
Runtime Composition is also Accepted and fixes the target dependencies,
configuration, task supervision, readiness, and shutdown, but those application
changes are not implemented.

## Run

```bash
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-api -- ./config.json
```

The configuration file names the environment variable containing the inbound
bearer token. The token itself must not be stored in JSON.

The following is the current implemented Bitcoin/Ethereum redb configuration,
not the accepted PostgreSQL/Solana target. Its minimal two-chain shape is:

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

Ethereum `eth_sendRawTransaction` is the exception to generic failover: it is
attempted once against the first configured endpoint. A retryable failure is
retained as an ambiguous exact envelope, and the Ethereum coordinator performs
explicit exact-hash reconciliation or byte-identical replay. Native-only
multi-endpoint operators should therefore keep endpoint 0 submission-ready.

The optional `usdc` object allowlists one exact contract for that Ethereum
network. Startup requires nonempty contract code and verifies the expected six
decimals. Until RPC endpoint identity is admitted and validated independently,
enabling `usdc` requires exactly one Ethereum RPC endpoint so chain identity,
canonical block selection, and contract probes cannot span different nodes.
Native-only Ethereum configuration may still use ordered endpoint failover.
HTTP callers never choose a token contract or decimals. Omitting `usdc` leaves
native ETH enabled without a USDC wallet family.

## Accepted PostgreSQL/Solana configuration target

The accepted-but-unimplemented root replaces per-chain database paths with one
PostgreSQL object and adds one optional Solana index with a singular endpoint:

```json
{
  "bind": "127.0.0.1:8080",
  "bearer_token_env": "PAYMENT_API_TOKEN",
  "tls_terminated_upstream": false,
  "postgres": {
    "url_env": "PAYMENT_POSTGRES_URL",
    "schema": "payment",
    "max_connections": 16
  },
  "indexes": {
    "solana": {
      "network": "local",
      "genesis_hash": "<canonical-Base58-genesis-hash>",
      "rpc": {
        "endpoint": "http://127.0.0.1:8899",
        "headers": [],
        "timeout_seconds": 15,
        "max_response_bytes": 67108864
      },
      "sync": {
        "confirmation_depth": 1,
        "reorg_retention": 100,
        "poll_millis": 1000,
        "batch_size": 256
      }
    }
  },
  "wallets": [
    {
      "id": "treasury-sol",
      "asset": "sol",
      "secret_env": "TREASURY_SOL_SEED",
      "start_position": 0
    }
  ]
}
```

The accepted objects are exact closed schemas. `postgres.schema` is a validated
lowercase identifier, and each connection pins that schema plus `pg_catalog`;
the URL's search path cannot override it. Startup validates the already-applied
schema without DDL. Database credentials, endpoint text, header values, and seed
contents are redacted from ordinary diagnostics.

Solana has exactly one endpoint and no transparent retry or failover. The target
rejects aliases, per-chain database fields, `start_height`, commitment, priority
fee, lag/reference/quorum, retry, and Memo-program controls rather than ignoring
them. Every configured import reads exactly one lowercase 64-character
hexadecimal Ed25519 seed from its named environment variable; generated SOL
wallets are process-lifetime only and are not restart-recoverable.

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
after startup terminates the current runtime rather than leave a silently stale
API.

Under the accepted-but-unimplemented Solana target, startup first verifies every
chain identity and the exact executable Memo-v3 account, then validates
PostgreSQL and imports configured wallets before synchronization and HTTP. A
runtime-fatal Solana indexer exit publishes not-ready and closes new admission.
With no guarded envelope the process exits; with a submitted or ambiguous
envelope, supervised shutdown waits without an automatic deadline while status
and indexing evidence remain available. After a fatal indexer exit, only
positive historical status can clear the guard in-process; force-kill explicitly
accepts the documented duplicate-payment risk.

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

The asset must have been configured at startup. The current implementation
accepts `btc`, `eth`, and `usdc`; `usdc` is available only when its contract is
configured. The accepted target adds `sol`, but that value is not implemented.
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

## Shared destination input

Both transaction POST endpoints reuse one exact destination object. It contains
only `encoding` and `text`; address-encoding values and chain-native text rules
remain chain-specific.

An unrecognized destination member, including a lag, reference-provider,
provider-role, quorum, sampling, fallback, or explicit no-reference control,
rejects the complete authenticated JSON body with `400 Bad Request` and the
generic `request body must match the documented JSON schema` message. This
happens before post-deserialization conversion or wallet delegation. A rejected
batch body therefore returns no accepted transaction IDs or failed index.
OpenAPI publishes `AddressInput` with `additionalProperties: false`.

## Native SOL account acquisition target

One native SOL single or batch send performs one endpoint-affine account
acquisition with no automatic retry, transparent transport retry, failover, or
chunking:

```text
getHealth()
  -> getSlot(confirmed) = F
  -> getMultipleAccounts(confirmed, base64, minContextSlot = F) = (C, values)
  -> getSlot(confirmed, minContextSlot = C) = U
  -> atomic eligibility and balance handoff at operation floor P = U
```

Source and destination addresses are deduplicated stably by their canonical
32-byte value in first-occurrence order. Each original transfer contributes its
source and then destination when that address has not appeared already. The
50-transfer public limit therefore produces at most 100 addresses and exactly
one `getMultipleAccounts` call. Results map by request position only after exact
cardinality validation and then map back to every original occurrence.

The request asks for full Base64 data without `dataSlice`. Explicit JSON `null`
alone represents absence. Existing accounts must contain valid lamports, owner,
executable, data, and total-space values. Data uses the exact
`[string, "base64"]` tuple and strict Base64 decoding; owner text is canonical
Base58 for exactly 32 bytes; and decoded data length equals the reported space.
The response context and complete payload structure validate before the
closing request. `C` must satisfy `C >= F`; the closing witness must return
`U >= C`. `F`, `C`, and `U` remain provisional until eligibility and source-
balance classification succeed; only then does `U` leave acquisition as `P`
atomically with the complete handoff.

An absent destination is eligible. An existing destination is eligible only
when it is non-executable, System-owned, and zero-data. Existing sources require
the same account shape; an absent source contributes zero lamports and fails
later balance sufficiency. A structurally valid but unsupported account is an
item-scoped error assigned to the earliest original occurrence using it. It
prevents the handoff and publishes no floor.

Timeout or cancellation at any await, oversized response, transport/HTTP/RPC
failure, malformed JSON/Base64/owner or account fields, cardinality mismatch,
data/space disagreement, below-floor context, or closing-witness failure aborts
the complete acquisition. It is index-free, publishes no slot floor, releases
every pre-envelope lexical source lease already held, leaves no background
acquisition, returns no transaction ID, accepted IDs, failed index, or
ambiguous ID, and performs no fee call, construction, signing, simulation, or
broadcast. It cannot release a coordinator-owned submitted or ambiguous
envelope guard. A new caller invocation starts without retained account facts
or a floor.

## Native SOL submission target

One process-local Solana coordinator admits a source at a time. A busy or
ambiguously guarded source rejects the complete new invocation as `503` before
account RPC or transaction work. A batch identifies the earliest original
occurrence using that source; a single send has no failed index.

Every non-self payment becomes one legacy transaction containing one System
Program native transfer followed by one Memo-v3 instruction with a fresh opaque
random 256-bit Base58 token. That token makes intentional identical occurrences
produce distinct signatures; it carries no payment/customer facts and is not an
HTTP idempotency key.

After the account-acquisition handoff, the complete single or batch operation
obtains a confirmed recent blockhash, constructs every message, obtains every
exact fee, checks cumulative `amount + fee` per source, signs and verifies every
distinct transaction, and simulates every exact signed transaction. Any
preparation failure causes zero broadcasts. The recent-blockhash lifetime uses
block height, not slot, and is checked before each broadcast.

Prepared transactions broadcast in original order with provider retries
disabled. The returned signature must equal the locally derived first signature
and means submitted, not confirmed. After an unknown response, the coordinator
may make at most two more byte-identical submissions, for three wire calls
total, only after signature-status and block-height checks. It never rebuilds or
re-signs an envelope after any item may have reached the network.

Once the first wire call begins, a timeout, disconnect, cancellation, RPC error,
malformed response, or returned-signature mismatch is ambiguous rather than a
proved failure. A single send returns `503` with only the locally derived
`ambiguous_transaction_id`. A batch returns `503` with the definitely
acknowledged prefix, the ambiguous occurrence's original `failed_index`, and
that ID; it does not attempt later items.

The accepted native SOL batch target therefore uses this shape:

```json
{
  "message": "Solana submission outcome is ambiguous",
  "transaction_ids": ["<accepted Solana signature>"],
  "failed_index": 1,
  "ambiguous_transaction_id": "<locally derived Solana signature>"
}
```

An ambiguous source remains blocked until status or canonical finalized history
proves observation, or blockhash expiry plus complete checkpoint-stable history
proves absence. Missing evidence may block it indefinitely. This state is not
durable: operators must run one active writer per source, callers must not
automatically retry an unknown logical payment, and response loss, restart,
failover, active-active writers, or a new invocation can double-pay because a
new invocation creates a new Memo token and exact transaction identity.

## Transaction request controls and precedence

Following any existing transport or authentication rejection, both transaction
POST routes reject a non-empty URI query string with `400 Bad Request` and:

```json
{"message":"transaction query parameters are not supported"}
```

That rejection occurs before JSON shape extraction, request conversion, or
wallet delegation. An empty query component has no semantic effect. Ordinary
HTTP, proxy, authentication, content-negotiation, and tracing headers remain
permitted, but no header controls transaction lag, reference selection,
commitment, retry, priority fee, or other send behavior.

The remaining public precedence is JSON exact-schema validation; batch
cardinality; wire-item conversion in original order; itemwise common validation
in original order; chain-specific complete preparation; then ordered broadcast.
For each batch occurrence, common validation checks its positive amount,
resolves its wallet, and checks family compatibility before advancing to the
next occurrence.

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

The top-level `SendFunds` body is exact: `destination` and string-typed
`amount` are its only properties and both are required. An unrecognized
top-level member, including a lag/reference control or wrapper, rejects the
complete authenticated body with `400 Bad Request` and the generic
`request body must match the documented JSON schema` message. Rejection occurs
before post-deserialization conversion, wallet lookup, or `Wallets::send`, and
the error contains no transaction-ID or failed-index metadata. OpenAPI
publishes `SendFunds` with `additionalProperties: false`, exactly those two
required properties, and the shared `AddressInput` reference.

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

The batch surface is:

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

Each `WalletTransfer` item is exact: required string `wallet_id`, shared
`AddressInput` `destination`, and required string `amount` are its only
properties. An unrecognized item property in any array position rejects the
complete authenticated body with `400 Bad Request` and the generic
`request body must match the documented JSON schema` message. No item prefix
is accepted, the error contains no transaction IDs or failed index, and
rejection occurs before post-deserialization request conversion or
`Wallets::send_all`. OpenAPI publishes `WalletTransfer` with
`additionalProperties: false`, exactly those three required properties, the
string-typed `wallet_id` and `amount`, and the shared `AddressInput` reference.

The enclosing `TransferRequest` root is also exact. Its only property is the
required `transfers` array, whose items reference `WalletTransfer`. A missing
or non-array `transfers`, invalid root JSON type, or unrecognized root property
rejects the complete authenticated body with the same generic `400` message,
no transaction IDs or failed index, and no post-deserialization conversion or
`Wallets::send_all` call. OpenAPI publishes the root with
`additionalProperties: false`, exactly that required array property and item
reference, and uses it as the batch operation's request body. This root-schema
closure is independent from the array policy below and does not imply item
uniqueness.

The `transfers` array contains from one through 50 items. An authenticated,
structurally valid `{"transfers":[]}` reaches the authoritative
`Wallets::send_all` guard and returns exactly `400 Bad Request` with:

```json
{"message":"at least one transfer is required"}
```

That collection-level failure has no `transaction_id`, `transaction_ids`, or
`failed_index`, invokes no registered sender, and produces no transaction or
chain-side external effect. Its SDK classification is `InvalidBatch`, and its
public rendering never invents item zero. OpenAPI publishes `minItems: 1`, no
`uniqueItems` or default array, and describes accepted IDs and a failed item
index as conditional metadata available only when a real item fails.

More than 50 items returns the index-free `400 Bad Request` collection error:

```json
{"message":"at most 50 transfers are allowed"}
```

That failure has no transaction IDs, failed index, or sender/RPC call. The HTTP
adapter applies the shared maximum before converting any item, while
`Wallets::send_all` remains the authoritative minimum-and-maximum guard for HTTP
and direct SDK callers. OpenAPI publishes `maxItems: 50`.

Every item's identity is its zero-based position in the authored array. The
entire public-to-sender path preserves exact order and multiplicity. Repeated
wallet IDs, destinations, amounts, and identical items remain distinct payment
occurrences; OpenAPI publishes no `uniqueItems` rule. Internal account/RPC reads
may deduplicate observations only when their results map back to every original
occurrence.

Its success response is `202 Accepted` with the submitted chain-native IDs:

```json
{"transaction_ids":["<transaction id>"]}
```

Required semantics are:

- validation, result mapping, and item-scoped failures preserve each original
  zero-based occurrence index;
- one request targets one exact wallet asset; mixed-family wallet IDs,
  including ETH and USDC on the same chain, are rejected before an external
  effect;
- Bitcoin may combine several source wallets into one transaction, reading all
  sources at one checkpoint, preserving distinct per-source change, signing
  each input with its owner, and creating one requested output per transfer;
- Ethereum builds separate transactions and broadcasts them in input order,
  reserving consecutive nonces per sender and reporting any accepted prefix if
  a later broadcast fails. Every Ethereum item is simulated, cumulatively
  balance-checked, and signed before the first envelope is submitted; and
- Solana builds one distinct transfer-plus-Memo transaction per occurrence,
  prepares, fee-checks, signs, and simulates the complete batch before its first
  broadcast, submits in input order, and stops with only the definitely
  acknowledged prefix at the first failure or ambiguity.

The one-process acceptance suite proves a two-transfer Bitcoin batch produces
one transaction/ID and a two-transfer Ethereum batch produces two IDs in input
order, with both results later visible through indexing. It also proves
multi-source Bitcoin signatures, an Ethereum failure after a submitted prefix,
and zero-broadcast rejection for mixed ETH/USDC families. Consecutive nonce
reservation and whole-batch Ethereum preflight are enforced by the coordinator
shared by the ETH and USDC providers. A retryable ambiguous submission retains
the exact envelope and blocks later sends from that address until exact-hash
reconciliation or byte-identical replay succeeds.

Those are current Bitcoin and Ethereum implementation claims; the equivalent
accepted native SOL batch and ambiguity evidence is still missing and is listed
in `docs/FEATURE_VALIDATION.md`.

That safety state lives only in the running API process. Operators must use one
active transaction writer per managed EOA; an unclean restart can require
manual reconciliation because this API does not persist outgoing operations.

The accepted-but-unimplemented Solana target likewise keeps its source guard
and exact envelope only in the running process. It requires one active writer
per source and forbids automatic client retry of an unknown logical payment;
restart or a new invocation can create a new Memo and double-pay.

If a sequential batch fails after a prefix was accepted, the error body keeps
the ordinary `message` and adds the accepted `transaction_ids` plus the
zero-based `failed_index` from the original request. If exact signed bytes may
have been submitted without a provable outcome, it also carries the locally
derived canonical `ambiguous_transaction_id`:

```json
{
  "message": "Ethereum submission outcome is ambiguous",
  "transaction_ids": ["<accepted transaction id>"],
  "failed_index": 1,
  "ambiguous_transaction_id": "<locally derived transaction id>"
}
```

A grouped transaction may represent several public occurrences with one
chain-native transaction. Its failure or ambiguity has no truthful single item
index, so the error must omit `failed_index` rather than inventing item zero. A
grouped ambiguity may still expose its locally derived
`ambiguous_transaction_id`. A single-send ambiguity likewise returns `503` with
that ID and without batch IDs or a failed index.

## Errors

Current errors return JSON with required `message` and may add
`transaction_ids` and `failed_index`. The accepted Public Transaction Semantics
target also adds optional `ambiguous_transaction_id` and makes failed-index
presence depend on a truthful item-scoped failure. The current status classes,
which the target retains, are:

| Status | Meaning |
|---|---|
| `400` | malformed request or cursor |
| `404` | asset is not configured or wallet ID is unknown |
| `409` | duplicate/conflicting composition state |
| `422` | deterministic transaction preparation or submission rejection |
| `503` | wallet/indexing unavailable or a submission outcome is ambiguous |
| `500` | an internal result could not be encoded |

Under the accepted target, batch transaction failures use `422` for
deterministic terminal failures and `503` for retryable or ambiguous failures.
Accepted IDs contain only the definitely acknowledged prefix. `failed_index`
is present only when one original occurrence truthfully failed. Presence of
`ambiguous_transaction_id` always produces `503`; it is reconciliation metadata,
not proof of submission or an idempotency key.

The chain transaction layer derives that ID from the exact locally signed
envelope before broadcast. Wallet/common-error conversion preserves it
unchanged and HTTP only renders it. Provider prose, a provider-supplied
candidate, or a returned ID that does not match the local envelope cannot
become reconciliation metadata.

## Current scope

The currently implemented BTC, ETH, and USDC routes cover wallet generation,
metadata, balance, history, ordinary sending, and chain-native batch sending.
Sections explicitly marked as accepted targets do not describe implemented
routes or fields.
There are no deposit, payment-state, accounting, collection, indexer-service,
or wallet-service routes.
