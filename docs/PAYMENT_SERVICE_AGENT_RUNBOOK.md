# Payment library validation runbook

There is a configured payment/deposit executable but no production custody stack.
Validate its components without running the executable or causing external
effects:

```bash
mac cargo test --locked -p payment-api
mac cargo clippy --locked -p payment-api --all-targets -- -D warnings
mac cargo test --locked -p system-tests --test ethereum_payment
```

The tests must prove that exact signed bytes are persisted, the transaction
watch is durable before broadcast, a lost response retries the same envelope,
event cursors survive restart, and reorg revisions correct confirmation state.
The system test additionally composes Payment HTTP and Indexer HTTP over
separate real temporary RocksDB databases and a deterministic Ethereum node.
Do not run live RPC or funded-key operations as part of this runbook.
