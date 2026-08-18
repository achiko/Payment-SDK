# Positive design examples

## Keep chain-native transaction ownership

Chain-specific construction and signing logic stays on the concrete chain type. The signer receives only a cryptographic payload.

```rust
let unsigned = ethereum.build(request).await?;
let digest = ethereum.signing_digest(&unsigned)?;
let signature = signer.sign(&key, &digest).await?;
let signed = ethereum.apply_signature(unsigned, signature)?;
```

## Model finite state explicitly

```rust
enum DepositState {
    AwaitingWatch,
    Watching,
    Confirmed,
}
```

An enum prevents unsupported string values and gives transitions a typed vocabulary.

## Keep tests beside their owner

Focused unit tests belong in the production module they validate. Integration tests belong under a kebab-case suite directory only when they exercise public APIs across crate boundaries.
