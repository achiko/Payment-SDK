# ADR-0002: Make wallet generation provider-owned

## Status

Accepted

## Date

2026-08-26

## Context

`Wallets::generate` is the chain-neutral application operation for creating a
wallet without returning its secret. It selects the registered family and
delegates construction to that family's `Provider`.

The current `Provider` contract requires `create(SecretBytes)` but gives
`generate()` a default implementation that always calls
`SecretBytes::generate_secp256k1()`. That default is correct for the existing
Bitcoin and Ethereum providers, but it silently places secp256k1 policy in the
chain-neutral wallet crate.

Native Solana wallets use Ed25519 and import one validated 32-byte secret seed.
A Solana provider could override the existing default, but any new provider
that forgets to do so would still compile. Because a valid secp256k1 scalar is
also 32 bytes, the mistake could appear to work when those bytes are later
interpreted as an Ed25519 seed.

The architecture requires concrete chains to own wallet construction, signing,
and native validation. Business and HTTP code must continue to call the same
chain-neutral operation and must never receive secret material.

## Decision

Make both `Provider::generate` and `Provider::create` mandatory object-safe
methods. Remove the secp256k1-generating default from the `Provider` trait.

The generation path will remain:

```text
Wallets::generate
    -> registered Provider::generate
    -> provider-selected native key generation
    -> the same provider's create and validation path
    -> Arc<dyn Wallet>
```

Each concrete provider owns its policy:

- Bitcoin and Ethereum explicitly generate a valid secp256k1 secret using the
  existing cryptography helper and then reuse their `create` path.
- Solana explicitly generates a valid 32-byte Ed25519 seed using its maintained
  chain-native dependency and then reuses its `create` path.
- `create` validates imported or generated secret material according to that
  provider's native rules before deriving an address or constructing a signer.

`SecretBytes` remains the opaque, zeroizing container used to move imported
secret material into its owner. It does not identify a curve or select a
generation policy. Generated secrets remain inside the provider and never
return to `Wallets`, the application, or an API caller.

The `Provider for Arc<T>` implementation must forward both required methods.
Generation failures must remain typed as wallet-generation errors and must not
register a wallet or indexing filter.

This decision does not make the shared secp256k1 `KeyPair` curve-polymorphic.
The Solana crate will own its Ed25519 signer and native message-signing rules.

## Consequences

### Positive

- Curve and seed-format selection become explicit responsibilities of every
  concrete provider.
- A newly added provider cannot silently inherit secp256k1 generation.
- `Wallets`, business code, and HTTP handlers remain chain-neutral.
- Adding or removing Solana does not add Ed25519 policy to generic wallet code.
- Import and generation converge on the same provider-owned validation path.

### Negative

- Every production provider and test fixture must implement `generate`.
- Bitcoin and Ethereum repeat a small generation and error-mapping block.
- The trait change requires coordinated updates before the workspace compiles.

### Neutral

- The existing secp256k1 generator may remain a generic cryptographic helper;
  invoking it as wallet policy moves to the concrete provider.
- This remains in-process development custody and does not add hardware-wallet,
  remote-signer, HSM, or KMS behavior.
- Selection of the exact maintained Solana SDK component is deferred to the
  dependency spike before the Solana crate is scaffolded.

## Alternatives considered

### Keep the default and override only Solana

Rejected. It is the smallest immediate code change, but it preserves a hidden
secp256k1 policy in the chain-neutral trait and lets future providers compile
without making an explicit key-generation decision.

### Add a curve method and switch in the generic default

Rejected. A generic `curve()` discriminator would give `sdk/wallets` a closed
cryptographic-policy table and require generic code changes for every new key
algorithm or seed format.

### Generate arbitrary 32-byte secrets generically

Rejected. Length alone does not establish protocol validity. Arbitrary bytes
can be an invalid secp256k1 scalar, and a generic byte generator cannot express
chain-native seed or keypair rules.

### Generate keys in the application or HTTP layer

Rejected. It would force chain or curve selection into business composition,
expose secret handling above the provider, and violate the stable
`Wallets::generate` boundary.

### Inject a generic secret-generator capability

Rejected for the current scope. A separate strategy object or associated secret
type adds ceremony to the object-safe provider and unified import boundary
without improving the three concrete providers' ownership.

### Generalize the shared key and signer implementation now

Rejected for this decision. Bitcoin and Ethereum sign precomputed secp256k1
digests, while Solana signs native message bytes with Ed25519. A universal key
object would erase protocol differences that the concrete chain must retain.

## Failure modes and required validation

- Omitting `generate` from any provider must be a compile-time error.
- Unavailable operating-system randomness must return a generation error and
  leave no wallet or filter registered.
- Bitcoin and Ethereum generation must still produce valid secp256k1 wallets
  with addresses derived from their generated signer.
- Solana generation must produce an Ed25519 wallet whose canonical address
  matches the generated signer's public key.
- Solana import must reject values other than its accepted 32-byte seed and
  must derive the same address for the same seed and scope.
- Provider fixtures must choose their generation behavior explicitly.
- Secrets must remain absent from responses, schemas, logs, errors, and ordinary
  `Debug` output.

## Approval boundary

Decision `S1.3` was explicitly approved on 2026-08-27. Acceptance records the
architecture decision; provider trait and implementation changes remain
separate implementation steps.

## References

- [Solana accounts](https://solana.com/docs/core/accounts)
- [Anza Solana SDK](https://github.com/anza-xyz/solana-sdk)
- `sdk/wallets/src/provider.rs`
- `sdk/wallets/src/wallets.rs`
- `packages/crypto/src/secret.rs`
- `sdk/chains/base/src/key_pair.rs`
- `sdk/chains/bitcoin/src/wallet/provider.rs`
- `sdk/chains/ethereum/src/wallet/mod.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/CONTRACTS.md`
