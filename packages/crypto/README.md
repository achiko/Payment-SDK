# crypto

Reusable cryptographic mechanisms with no payment or blockchain business logic.

The package owns key validation and zeroization, public-key encoding, generic
scalar tweaks, and signature algorithms. Callers own protocol-specific hashing,
domain-separation tags, transaction rules, address formats, and custody policy.

It intentionally exposes a concrete in-memory secp256k1 key rather than a
wallet, signer service, RPC client, or chain abstraction.
