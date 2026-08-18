# Wallet HTTP library

`apps/wallet` is a stateless Axum runtime for already initialized
`Arc<dyn wallets::Wallet>` values. `Service::with(id, wallet)` registers a
wallet and `router`, `serve`, or `run` exposes:

```text
GET  /health/live
GET  /health/ready
GET  /v1/wallets/{id}
GET  /v1/wallets/{id}/balance
GET  /v1/wallets/{id}/history
POST /v1/wallets/{id}/transactions
PUT  /v1/wallets/{id}/transactions/{transaction_id}
```

Wallet construction belongs to application startup through `wallets::Wallets`
and concrete providers. The checked-in executable can compose native Bitcoin
and Ethereum wallets from explicit environment configuration. It decodes each
private key directly into zeroizing `SecretBytes`, constructs focused RPC
capabilities, and uses the remote Indexer adapter for history and Bitcoin UTXO
selection. Ordered RPC and Indexer endpoint lists provide retry-aware failover.
It owns no database and never prints configuration or secret values.

With no chain variables it starts live and reports not-ready. Setting any
variable for a chain enables strict validation for that chain; partial
configuration fails startup. This makes an accidentally half-configured wallet
different from an intentionally empty development process.

`POST /transactions` prepares and returns a serializable `SignedTransaction`;
it does not broadcast. The caller must durably persist that response and
register its Indexer watch before using transaction-addressed `PUT` to submit
the same signed bytes. Retrying the `PUT` never rebuilds or re-signs. The path
ID must equal the signed body's ID and a divergent node response is rejected.
Durable command deduplication remains PS ownership; a stateless WS does not
pretend to remember idempotency keys.

The runtime owns bind, mandatory bearer authentication, bounded request bodies,
concrete adapter composition, graceful Ctrl+C shutdown, and truthful
readiness. Health endpoints are unauthenticated and contain no details. Plain
HTTP is loopback-only; a non-loopback listener declares upstream TLS
termination. The deployment still owns TLS, secret injection/rotation,
endpoint selection, and rate limits. Environment key input is the initial
local-key implementation, not a claim of production custody.
