# ADR-0005: Use checked `u64` lamports for native SOL amounts

## Status

Accepted

## Date

2026-08-27

## Context

The public wallet surface receives an exact `base::Decimal`, while a native
Solana System Program transfer carries its amount as an unsigned 64-bit number
of lamports. One SOL is exactly `1_000_000_000` lamports.

The boundary must reject values that cannot be represented exactly. Rounding a
sub-lamport amount would either send less than requested or turn a positive
request into a zero-value instruction. Binary floating point would also make
the result depend on approximation rather than the caller's exact decimal.

The existing `Decimal::to_atomic_u64` operation already performs checked,
base-10 conversion and distinguishes negative amounts, excess precision, and
overflow. Bitcoin uses the same primitive for its chain-owned `Satoshi` value.
Solana does not require a new generic amount abstraction.

## Decision

The Solana chain crate will own a native `Lamport(u64)` value. Its conversion
from a public send amount will:

1. require the decimal amount to be strictly greater than zero;
2. convert with exactly nine fractional decimal places;
3. reject any non-zero fraction smaller than one lamport;
4. reject any value greater than `u64::MAX` lamports; and
5. return the exact integer without rounding, truncation, saturation, or
   floating-point conversion.

In source terms, the conversion contract is equivalent to:

```rust,ignore
let lamports = amount.to_atomic_u64(9)?;
if lamports == 0 {
    return Err(InvalidAmount);
}
```

The chain boundary repeats this validation even though `Wallet::send` and
`Wallets::send_all` already reject non-positive amounts. Direct builder use,
snapshot restoration, and future internal callers must not bypass the native
amount invariant.

Canonical decimal normalization remains valid. For example,
`1.0000000000 SOL` is exactly one SOL and loses no value when converted. A
value such as `0.0000000001 SOL` is rejected because it contains a non-zero
sub-lamport fraction.

The upper conversion endpoint, `18_446_744_073.709551615 SOL`, maps to
`u64::MAX` lamports and is representable at this boundary. A later fee and
balance decision may still reject it when checked addition of the network fee
cannot fit or the source lacks funds.

Native amounts received from Solana RPC or decoded instructions remain exact
`u64` lamports. Wallet-facing balance and history presentation converts them
with `Decimal::from_atomic(value.into(), 9)`. Canonical indexing continues to
store native SOL movements and fees as non-negative scale-zero atomic lamports,
as accepted in S1.5.

All conversion failures map to the existing invalid-amount error category.
They occur before destination-state RPC, blockhash acquisition, fee quotation,
signing, simulation, or broadcast.

## Scope boundary

S1.6.1 decides only the amount representation and exact conversion. It does
not decide:

- destination account eligibility;
- RPC endpoint or commitment affinity;
- recent blockhash acquisition or expiry;
- legacy message construction;
- fee quotation or cumulative balance sufficiency;
- Ed25519 signing or transaction identity;
- simulation, broadcast, retry, or ambiguity handling; or
- ordered batch behavior.

Each item remains a separate approval-sized decision.

## Alternatives considered

### Convert through `f64`

Rejected. Binary floating point cannot represent every decimal SOL amount
exactly and would violate the project's exact-money requirement.

### Round or truncate sub-lamport values

Rejected. The submitted value would differ from the caller's requested value.

### Keep arbitrary-precision units until instruction encoding

Rejected. The native instruction boundary is `u64`; delaying the range check
would allow invalid intent to reach later RPC or signing stages.

### Add a generic atomic-amount type

Rejected. Atomic width and unit meaning are chain-native. Bitcoin already owns
`Satoshi`, Ethereum owns `Wei`, and Solana should own lamports without changing
the generic wallet contract.

## Consequences

- Every accepted public SOL amount has one exact native representation.
- Lamport quantities cannot be confused with slot or block-height integers;
  contextual transaction models still distinguish transfer value from fee.
- No generic wallet, transaction, indexing, persistence, or HTTP amount type
  changes for S1.6.1.
- Amount conversion remains pure and testable without RPC or signing.

## Validation requirements

Focused tests must prove:

- `1 SOL` becomes `1_000_000_000` lamports;
- `0.000000001 SOL` becomes one lamport;
- canonical extra trailing zeros remain exact;
- zero and negative values fail;
- a non-zero sub-lamport fraction fails;
- `u64::MAX` lamports round-trips exactly;
- one lamport above `u64::MAX` fails; and
- every failure happens before any RPC, signing, or broadcast double is called.

## Approval boundary

Decision `S1.6.1` was explicitly approved on 2026-08-27. Acceptance records the
native amount decision only; it does not authorize Solana source, wallet,
transaction, RPC, dependency, API, or test implementation.

## References

- [Anza native SOL token units](https://docs.rs/solana-native-token/latest/solana_native_token/)
- [Anza System Program transfer](https://docs.rs/solana-system-interface/latest/solana_system_interface/instruction/fn.transfer.html)
- `sdk/chains/base/src/decimal.rs`
- `sdk/chains/bitcoin/src/lib.rs`
- `sdk/chains/ethereum/src/lib.rs`
- `sdk/wallets/src/wallet.rs`
- `sdk/wallets/src/wallets.rs`
- `docs/SYSTEM_REQUIREMENTS.md`
- `docs/adr/0004-derive-native-sol-history-from-system-transfers.md`
