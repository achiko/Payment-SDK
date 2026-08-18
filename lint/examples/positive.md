# Positive design examples

## Small reusable capability

```rust
trait Addresser {
    fn address(&self) -> Address;
}
```

The trait names one reusable capability and stays independent of concrete
chains.

## Chain-native execution

Concrete transaction construction remains in its owning chain. Generic wallet
code invokes a small transaction capability and never interprets scripts,
UTXOs, nonces, gas, envelopes, or signatures.

## One composition root

`apps/api` constructs concrete RPC clients, embedded indexers, storage, and
wallet providers once. Handlers depend on wallet and indexing abstractions.
