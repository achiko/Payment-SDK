# Negative design examples

## Do not replace chain-native transactions with one universal model

```rust
struct Transaction {
    from: String,
    to: String,
    amount: u64,
}
```

Bitcoin inputs, outputs, scripts, and fee rules are not interchangeable with Ethereum nonces, gas, envelopes, and receipts. Keep these models in their owning chain crates.

## Do not encode workflow state as strings

```rust
struct Deposit {
    state: String,
}
```

String state admits invalid values and hides the valid transition set. Use a domain enum.

## Do not ignore fallible operations

```rust
fn persist(store: &Store, value: Value) {
    store.write(value);
}
```

Return or deliberately handle the result so persistence failure cannot be mistaken for success.
