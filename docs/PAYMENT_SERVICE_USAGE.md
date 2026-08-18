# Payment library usage

The checked-in `apps/api` crate provides an injected `Service` and a configured
`payment-api <config.json>` executable for Bitcoin/Ethereum payments. There is
no repository-owned production custody server. The executable opens payment
RocksDB, connects to the configured remote Indexer and chain RPCs, constructs
wallets from 32-byte hexadecimal keys named by environment variables, enforces
bearer authentication, and supervises HTTP plus reconciliation.

Both `indexer.endpoints` and each Bitcoin/Ethereum wallet's `rpc_urls` are
ordered, non-empty endpoint lists. Requests try the next endpoint only after a
retryable transport or HTTP-status failure. A JSON-RPC protocol error or remote
method error is authoritative and does not silently fall through to another
node. Put the preferred node first; every configured node must serve the same
chain and network.

```rust,ignore
use std::sync::Arc;

use payment_api::Payments;

let indexer: Arc<dyn indexing::Indexer> = Arc::new(indexing_remote);
let payments = Payments::new(indexer, payment_storage)
    .with("bitcoin-hot", bitcoin_scope, bitcoin_wallet)?
    .with("ethereum-hot", ethereum_scope, ethereum_wallet)?;
```

Later business logic selects only the configured wallet ID and protocol-neutral
address/amount inputs. It does not import a concrete chain implementation.

```rust,ignore
let payment = payments.request(request).await?;
```

The request flow persists exact signed bytes, registers the indexer watch, and
then broadcasts. A background task should call `Payments::reconcile` for each
configured scope. That consumes durable index events and advances or corrects
payment state after confirmations and reorgs.

For custom embedding, create `payment_api::Config` and an
`http_kit::server::Config`, construct
`payment_api::Service::new(config, Arc::new(payments), server)`, and call `run`,
`run_until`, or `run_on`. The service supervises HTTP and reconciliation with
truthful readiness. The shared HTTP config enforces authentication and request
limits. The embedding application remains responsible for TLS, secrets,
custody, operator policy, and selecting the `Storage` implementation.

For the checked-in executable, pass the JSON configuration path as its only
argument after setting the referenced key and optional Indexer-token
environment variables:

```bash
mac cargo run --locked -p payment-api -- ./payment.json
```

This can contact configured nodes and broadcast funded transactions. Do not run
it with live endpoints or funded keys without explicit review. With the
singular optional deposit configuration it also serves address/watch,
observation, balance/history, and collection planning/execution routes for
Bitcoin native, Ethereum native, or ERC-20. ERC-20 may name a same-scope native
gas wallet. `POST /v1/collections` accepts only stable IDs and a timestamp; the
server derives the asset, mode, destination, amount, policy, and spend resources
from configuration plus durable PS/IX state. Multiple simultaneous deposit
scopes are not exposed. Bearer authentication is mandatory; TLS termination
must be supplied by deployment.

There is no checked-in payment backup, restore, or migration command. Do not
follow older instructions that reference `custody-worker` or the removed
local-stack scripts.

Validation for this layer is exercised through the `payment-api` package's
integration tests:

```bash
mac cargo test --locked -p payment-api
```
