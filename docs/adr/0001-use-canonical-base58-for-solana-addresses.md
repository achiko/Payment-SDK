# ADR-0001: Use canonical plain Base58 for Solana addresses

## Status

Accepted

## Date

2026-08-26

## Context

Payment-SDK represents protocol-neutral address bytes with `base::Address` and
lets each concrete wallet own the reversible conversion to and from the
user-facing `wallets::AddressText` boundary. `AddressText` identifies its text
encoding with `AddressEncoding`.

Solana account addresses are 32-byte values displayed as Base58 strings. This
is plain Base58: it is not Bitcoin Base58Check and does not carry a checksum or
a network prefix. The current `AddressEncoding` vocabulary supports
`Base58Check`, `Bech32`, `Bech32m`, and `Hex`, but has no value that accurately
describes Solana's canonical address text.

Using `Base58Check` for Solana would mislabel and alter an external protocol
contract. Using `Hex` would produce non-native addresses that do not match
wallets, explorers, RPC methods, or transaction messages.

## Decision

Add a distinct `AddressEncoding::Base58` value for canonical plain Base58.

The Solana wallet's address formatter and parser will:

- encode the complete 32-byte Solana account address as plain **Base58**;
- decode Base58 input and require exactly 32 bytes;
- reject malformed or non-canonical text by requiring a decode/re-encode
  round trip to reproduce the input exactly;
- store the decoded bytes in the existing protocol-neutral `base::Address`;
- keep chain and network identity in the existing wallet/indexing scope rather
  than embedding Payment-SDK-specific metadata in the address text.

This decision specifies representation only. A syntactically valid Solana
address is not necessarily a safe native-SOL recipient. On-curve checks,
account existence, and owner classification belong to a separate native-SOL
destination-policy decision.

## Consequences

### Positive

- Public addresses match the canonical representation used by Solana wallets,
  RPC methods, explorers, and transaction messages.
- Plain Base58 and Bitcoin Base58Check remain explicit, non-interchangeable
  protocol contracts.
- Parsing is deterministic, reversible, local, and independent of an RPC
  endpoint.
- The shared address remains opaque bytes; Solana-specific validation stays in
  the Solana chain crate.

### Negative

- Plain Base58 has no checksum, so syntactically valid typing mistakes cannot
  be detected by checksum validation.
- Adding an `AddressEncoding` variant requires every exhaustive match to be
  reviewed and updated.
- Canonical syntax validation alone cannot prevent SOL from being sent to a
  program, token account, mint, or inaccessible PDA.

### Neutral

- The same address text may appear on different Solana clusters. Payment-SDK's
  configured chain/network scope remains responsible for that distinction.
- Dependency selection for Base58 operations is deferred to implementation;
  implementation must use a maintained protocol library rather than a custom
  codec.

## Alternatives considered

### Reuse `AddressEncoding::Base58Check`

**Rejected**. Base58Check adds checksum/version semantics that are not part of a
Solana account address and would make the resulting text incompatible with
Solana's native interfaces.

### Expose Solana addresses as hexadecimal

Rejected. Hex is reversible but is not Solana's canonical external address
format and would force callers to translate every wallet, RPC, and explorer
address.

### Treat the encoding as an unspecified string

Rejected. It would weaken the public boundary, permit ambiguous parsing, and
remove the ability to reject an address tagged with the wrong encoding before
chain-specific parsing.

### Keep Base58 entirely private to the Solana crate

Rejected for the public address boundary. The Solana parser remains
chain-owned, but `AddressText` must accurately identify the encoding it exposes
to chain-neutral callers.

## Failure modes and required validation

- Invalid Base58 characters must be rejected.
- Decoded values other than 32 bytes must be rejected.
- Empty input must be rejected.
- Canonically encoded 32-byte values must round-trip without change.
- Base58 text must never be parsed as Base58Check, or vice versa.
- Logs and errors may include the public address but must never include secret
  seed or private-key material.
- Destination-safety validation must be tested separately before native SOL
  transfer submission is enabled.

## Approval boundary

Decision `S1.2` was explicitly approved on 2026-08-27. Acceptance records the
address-representation decision only; it does not authorize Solana address,
wallet, RPC, transaction, or API implementation.

## References

- [Solana account structure](https://solana.com/docs/core/accounts/account-structure)
- [Solana RPC JSON structures](https://solana.com/docs/rpc/json-structures)
- [Solana native-payment address verification](https://solana.com/docs/payments/send-payments/verify-address)
- `sdk/wallets/src/address.rs`
- `sdk/chains/base/src/address.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
