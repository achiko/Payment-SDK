# Payment orchestration

## Current status

`apps/api` provides reusable payment orchestration, an Axum router, an injected
`Service`, and a `payment-api <config.json>` executable. The executable composes
outgoing Bitcoin/Ethereum payments and one optional finite deposit scope for
Bitcoin native, Ethereum native, or ERC-20. It requires bearer authentication
and supports trusted upstream TLS termination, but contains no production
custody service, policy engine, migration command, HA design, or complete
exchange runtime.

## Composition

`Payments` is constructed with an `Arc<dyn Indexer>` and a durable `Storage`
implementation. Each wallet is registered under an application-owned ID with
an exact `IndexScope`; a scope mismatch fails instead of silently routing a
transaction to another chain or network.

Business code can remain protocol-neutral:

```rust,ignore
let payments = Payments::new(indexer, storage)
    .with("treasury-btc", bitcoin_scope, bitcoin_wallet)?
    .with("treasury-eth", ethereum_scope, ethereum_wallet)?;

let payment = payments.request(request).await?;
```

Concrete wallets own address validation, transaction construction, signing,
and broadcast validation. The payment layer owns durable ordering and
recovery.

### Executable composition

`Runtime::build(RuntimeConfig)` opens payment RocksDB, creates one
multi-endpoint `indexing_http::Remote`, constructs every configured concrete
wallet, binds each wallet to its exact index scope and asset, and constructs the
supervised `Service`. Ethereum wallets may select native ETH or an ERC-20 by
canonical contract and decimals. `Runtime::run()` then binds authenticated HTTP
and runs payment/deposit reconciliation.

The JSON configuration contains the bind address, payment database path,
Indexer endpoints and limits, reconciliation policy, and tagged Bitcoin or
Ethereum wallet records. It references secrets by environment-variable name:
the Indexer bearer is optional and private keys must decode from hexadecimal to
exactly 32 bytes. Secret values never belong in the JSON document. Bitcoin
startup verifies Core network, genesis, and readiness before serving; Ethereum
uses its configured chain ID and transaction limits.

Indexer `endpoints` and chain `rpc_urls` preserve configuration order. Their
generic HTTP/JSON-RPC adapters advance to the next endpoint only for retryable
transport or status failures; JSON-RPC protocol and remote-method failures do
not fail over. Concrete chain validation still rejects a reachable node with a
different network, genesis block, or chain ID.

This is a real composition root, but deliberately a bounded one. An optional
`DepositConfig` selects exactly one configured wallet/asset and a finite set of
environment-referenced local keys. The runtime then composes address issuance,
durable watch registration, observation, balance/history routes, collection
planning, and execution. Bitcoin native uses UTXO batching; Ethereum native
uses account transfer; ERC-20 uses token-with-gas and may select a same-scope
native gas wallet. Collection policy and the master destination are bound by
configuration, not supplied by the request. Multiple simultaneous deposit
scopes remain unavailable. TLS termination and production custody remain
deployment responsibilities.

Deposit creation does not accept a key purpose. PS derives a stable address
operation ID from the deposit ID and walks the configured keys in canonical
purpose order. Each address can belong to only one durable deposit; occupied
candidates are skipped and exhaustion is explicit. Retrying the same deposit
reads its durable address and birthday, so it never allocates again or changes
the IX watch.

Authenticated collection HTTP is deliberately small: `POST /v1/collections`
derives and reserves a collection, `GET /v1/collections/{id}` reads its public
state, and `POST /v1/collections/{id}/execute` advances its next durable leg.
Planning accepts only `id`, `job_id`, `deposit_ids`, and `created_at`, with an
`Idempotency-Key` equal to `id`; responses exclude signed bytes and UTXO
evidence. The server binds scope, asset, transaction model, master destination,
and policy from configuration. It derives account amounts from the durable
ledger head. For UTXO batches it loads canonical output pages at one stable
snapshot, applies confirmation/coinbase maturity and participant/input limits,
rejects duplicate or changing resources, and atomically fences ledger heads and
selected outputs. Repeating the identical command replays the same durable
collection; reusing its identity with a changed body or job conflicts.

## Durable lifecycle

```text
Requested -> Prepared -> Watched -> Submitted -> Confirmed
```

Preparation returns `base::SignedTransaction`: a version, chain-owned kind,
canonical transaction ID, and exact signed envelope. The payment repository
stores that value before any broadcast. It then registers a transaction watch
with a caller-owned idempotency key and stores the validated watch receipt.
Only after those writes does it invoke the wallet's `Broadcaster`.

A retry reuses the persisted envelope, ID, and watch identity. It does not
rebuild or sign another transaction after a lost response.

Before the first broadcast, `Payments` reads the indexer's canonical
`Checkpoint` and registers the transaction watch from that height. It rejects
an unavailable checkpoint or a receipt with different boundaries. The durable
order is therefore prepared transaction, checkpoint, watch, then broadcast;
the payment never falls back to a genesis birthday.

## Confirmation and reorgs

`Payments::reconcile(scope, limit)` reads `Observer` pages. Payment evidence
and the per-scope event cursor commit atomically through `ReconcileStore`, so
restart replay is idempotent. Sufficient confirmation/finality moves a payment
to `Confirmed`. A later reorg revision is appended as evidence and can return
the payment to `Submitted`.

Wallets do not poll receipts and payment requests do not sleep waiting for
confirmation. Indexing is the single confirmation source.

## HTTP library

`apps/api::router` and `apps/api::serve` expose the current payment operations
and health endpoints around an already composed `Payments` value. The library
does not choose authentication, TLS, rate limits, custody, or operator policy.
The executable resolves configured secret environment variables only at its
application composition boundary.

An embedding host can add the typed `Deposits` and `Sweeps` capabilities to
`Service`. They expose deposit open/resume/get/list, current balance, immutable
ledger history, and account, token-with-gas, and UTXO collection execute/status
routes. Deposit creation requires a caller-owned
`Idempotency-Key`; collection execution requires that header to equal the
durable collection ID. Scope comes from application composition rather than an
HTTP field. Responses deliberately omit key identifiers, key-purpose metadata,
signed envelopes, and reserved-output evidence.

`GET /v1/deposits/{id}/balance` returns the latest complete absolute ledger
snapshot: `received`, `confirmed`, `balance`, `collected`, and `accounted`.
`GET /v1/deposits/{id}/history` returns the same complete snapshots with their
immutable causes and an opaque entry cursor. Both resources include the asset
identity, and every amount is an exact decimal string in that asset's atomic
units. Reorgs append correcting entries; they never rewrite earlier history.

`authenticated_gateway` applies the generic `packages/http` bearer middleware,
request-body limit, and detail-free health endpoints. Strict construction fails
without a bearer token or an explicitly declared application authorizer. The
route handlers retain no signing or persistence rules: they delegate the
watch-before-return and exact-envelope retry behavior to `Deposits` and
`Sweeps`.

`apps/api::Service` validates that configured reconciliation scopes exactly
match injected wallets, supervises HTTP plus periodic reconciliation, and
supports graceful shutdown. Readiness becomes true only after every configured
scope reconciles successfully and is cleared by a later failure.

## Persistence compatibility

The current payment library exposes no database backup, restore, or migration
command. A future persisted-schema or policy conversion requires an explicitly
approved recoverable operator design. Ordinary payment and indexing traits must
not expose physical migration mechanics.

## Evidence

Focused integration tests cover watch-before-broadcast ordering, recovery from
a lost submission response without resigning, confirmation, reorg correction,
restart cursor persistence, and replay idempotency. These tests prove the
library behavior they execute; they do not prove production custody, a public
deployment, HA, or live-chain operation.

The cross-service Ethereum system test additionally runs the concrete wallet,
mock JSON-RPC node, Indexer HTTP runtime, Payment HTTP surface, reconciliation,
restart, and canonical reorg correction against separate temporary RocksDB
databases. It is strong local composition evidence, not production or
live-network evidence.

The Bitcoin collection runtime acceptance test composes `Runtime::build`, the
concrete Bitcoin wallet/provider and resolver, deterministic Bitcoin RPC, the
real Indexer Service and two RocksDB databases. It opens and watches deposits,
observes confirmed outputs, plans the same two-input UTXO batch twice, verifies
a changed replay conflicts, restarts Payment Service, executes the persisted
collection, and inspects the exact transaction submitted to
`sendrawtransaction`:

```bash
mac cargo test --locked -p system-tests --test collection_runtime
```

This is deterministic loopback acceptance, not funded live-node evidence.
