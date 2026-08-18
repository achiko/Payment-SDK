# Bitcoin payment composition sketch

A future live regtest acceptance must compose Bitcoin Core, a concrete Bitcoin
wallet provider, the indexer runtime, and durable payment storage. The required
ordering is:

```text
prepare exact signed transaction
  -> persist it
  -> register and persist the transaction watch
  -> broadcast exact bytes
  -> index blocks
  -> reconcile revision events to confirmation
```

The current repository does not provide a complete executable for this flow.
Use the deterministic indexer and payment integration tests as the available
offline evidence; do not infer authorization to broadcast funds.
