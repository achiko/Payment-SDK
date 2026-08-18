# Payment HTTP manual testing

The previous Postman procedure targeted a removed custody stack and is no
longer a runnable contract. The current `payment-api <config.json>` executable
can serve payment, deposit, ledger-history, sweep, and health routes after
composing its configured wallets, Indexer client, RPC clients, and RocksDB.
Bearer authentication is required, while TLS termination and production
custody remain deployment responsibilities; this document does not recommend
manual live-network testing.

Use the deterministic server integration tests for the current HTTP surface:

```bash
mac cargo test --locked -p payment-api --test server
```

The composed Bitcoin collection acceptance test exercises authenticated
planning, replay/conflict handling, restart, and execution over loopback RPC
services and real temporary RocksDB:

```bash
mac cargo test --locked -p system-tests --test collection_runtime
```

Use the cross-service Ethereum acceptance test for composed wallet, broadcast,
Indexer HTTP, Payment HTTP, persistence, restart, and reorg behavior:

```bash
mac cargo test --locked -p system-tests --test ethereum_payment
```

Neither command contacts a public network or authorizes a funded broadcast.
