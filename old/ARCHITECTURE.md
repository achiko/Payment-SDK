# Target Architecture

## Status

This document is the architectural direction for the workspace. It is
normative: when the current prototype and this document disagree, this
document describes the intended design.

The current crates are an experiment and are not the desired final layout.
Migration should happen only after the contracts and dependency boundaries in
this document are agreed upon.

## Project goals

The SDK is intended to support an exchange payment system that can:

- create wallets and derive receiving addresses;
- start watching an address from its creation height;
- synchronize blocks from the earliest wallet height to the chain tip;
- find and persist transactions affecting watched addresses;
- report wallet and address balances;
- construct, sign, and broadcast transactions;
- collect or sweep funds from individual addresses or whole wallets;
- add another chain without teaching the generic layers about that chain.

## Desired workspace structure

```text
payment-sdk/
├── Cargo.toml                       # Virtual workspace only
│
├── apps/                            # Executables and composition roots
│   └── <binary-name>/
│       ├── Cargo.toml
│       └── src/main.rs
│
├── sdk/                             # Reusable product/blockchain capabilities
│   ├── chains/
│   │   ├── contract/               # Contract exposed by every chain
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── wallet.rs
│   │   │       ├── address.rs
│   │   │       ├── balance.rs
│   │   │       ├── transfer.rs
│   │   │       └── lib.rs
│   │   │
│   │   ├── bitcoin/                # All Bitcoin-specific logic
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── address.rs
│   │   │       ├── wallet.rs
│   │   │       ├── transaction/
│   │   │       │   ├── builder.rs
│   │   │       │   ├── unsigned.rs
│   │   │       │   ├── signed.rs
│   │   │       │   └── sighash.rs
│   │   │       ├── indexer/
│   │   │       ├── rpc/
│   │   │       └── lib.rs
│   │   │
│   │   └── ethereum/               # All Ethereum-specific logic
│   │       ├── Cargo.toml
│   │       └── src/
│   │           ├── address.rs
│   │           ├── wallet.rs
│   │           ├── transaction/
│   │           │   ├── builder.rs
│   │           │   ├── unsigned.rs
│   │           │   └── signed.rs
│   │           ├── indexer/
│   │           ├── rpc/
│   │           └── lib.rs
│   │
│   ├── transactions/
│   │   ├── utxo/                   # Reusable UTXO construction algorithms
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── builder.rs
│   │   │       ├── input.rs
│   │   │       ├── output.rs
│   │   │       ├── selection.rs
│   │   │       ├── fee.rs
│   │   │       └── change.rs
│   │   │
│   │   └── account/                # Reusable account-model construction
│   │       ├── Cargo.toml
│   │       └── src/
│   │           ├── builder.rs
│   │           ├── transfer.rs
│   │           ├── nonce.rs
│   │           └── fee.rs
│   │
│   ├── signing/
│   │   ├── contract/               # Chain-independent signer contract
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── signer.rs
│   │   │       ├── key.rs
│   │   │       ├── request.rs
│   │   │       ├── signature.rs
│   │   │       └── lib.rs
│   │   │
│   │   ├── local/                  # Local implementation of signer contract
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │
│   │   └── trezor/                 # Trezor implementation of signer contract
│   │       ├── Cargo.toml
│   │       └── src/
│   │
│   ├── indexing/                   # Chain-independent sync engine and types
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── indexer.rs
│   │       ├── source.rs
│   │       ├── block.rs
│   │       ├── checkpoint.rs
│   │       ├── reorg.rs
│   │       └── lib.rs
│   │
│   └── storage/                    # Backend-independent storage contract
│       ├── Cargo.toml
│       └── src/
│           ├── storage.rs
│           ├── namespace.rs
│           ├── batch.rs
│           ├── transaction.rs
│           └── lib.rs
│
└── packages/                       # General-purpose, transferable packages
    ├── http/
    │   └── Cargo.toml
    ├── json-rpc/
    │   └── Cargo.toml
    ├── transport/
    │   └── Cargo.toml
    └── telemetry/
        └── Cargo.toml
```

Every leaf containing a `Cargo.toml` is a Cargo package. `apps`, `sdk`, and
`packages` are architectural namespaces; packages must not be placed directly
at the repository root.

`sdk` is intentionally a role-based name. The layer contains both
chain-specific code and chain-independent but blockchain-oriented components,
so narrower names such as `crypto`, `ledger`, or `protocols` would be
inaccurate.

## Dependency direction

The dependency direction must remain one-way:

```text
apps
  ↓
sdk/chains
  ↓
sdk/{transactions, signing, indexing, storage}
  ↓
packages
```

Direct relationships are expected to look like this:

```text
apps/<binary>
├──► sdk/chains/bitcoin
├──► sdk/chains/ethereum
├──► sdk/signing/local or sdk/signing/trezor
└──► selected storage implementation, when one exists

sdk/chains/bitcoin
├──► sdk/chains/contract
├──► sdk/transactions/utxo
├──► sdk/signing/contract
├──► sdk/indexing
├──► sdk/storage
└──► packages/{json-rpc,http,...}

sdk/chains/ethereum
├──► sdk/chains/contract
├──► sdk/transactions/account
├──► sdk/signing/contract
├──► sdk/indexing
├──► sdk/storage
└──► packages/{json-rpc,http,...}

sdk/signing/local  ──► sdk/signing/contract ──► packages only
sdk/signing/trezor ──► sdk/signing/contract ──► packages only
sdk/indexing       ──► sdk/storage            ──► packages only
packages/*         ──► packages/* only
```

In abstraction order this is `storage -> indexing -> bitcoin`; in Cargo
dependency notation, where `A -> B` means “A depends on B,” it is
`bitcoin -> indexing -> storage`.

Dependencies between components at the same layer must still be deliberate.
Folder depth alone does not authorize a dependency.

## Ownership rules

### Applications are composition roots

`apps/` contains executable entry points. An application chooses concrete
chains, signers, transports, and eventually a storage implementation, then
injects them through contracts.

Applications may glue components together. They must not become the owner of
Bitcoin rules, Ethereum rules, signer implementations, indexing algorithms, or
generic infrastructure.

Application directories must be named after their actual executable role,
such as `api`, `worker`, or `cli`. Do not invent a vague name such as
`payment-service` merely to hold composition code.

### A chain owns everything specific to that chain

All Bitcoin-specific behavior belongs under `sdk/chains/bitcoin/`. All
Ethereum-specific behavior belongs under `sdk/chains/ethereum/`.

This includes:

- addresses and address encoding;
- chain wallet behavior;
- concrete RPC methods, request types, and response types;
- transaction serialization and chain-specific fields;
- sighash or signing-payload calculation;
- script, witness, and signature placement;
- block decoding and transaction interpretation;
- chain-specific indexer integration;
- fee rules and validation that are unique to the chain.

The deletion test is mandatory: deleting `sdk/chains/bitcoin/` must remove all
Bitcoin-specific code. It must not remove generic signing, UTXO selection,
indexing, storage, HTTP, or JSON-RPC code.

Each chain implements the contracts in `sdk/chains/contract/`. Prefer small
capability contracts for wallets, addresses, balances, transfers, and
indexing over one unconstrained god trait.

### Signing is independent from chains

A signer represents control of a key and cryptographic signing capability. It
must not know about Bitcoin, Ethereum, wallets, UTXOs, accounts, JSON-RPC, or
indexers.

The signer contract may expose operations such as:

- obtaining a public key;
- signing an arbitrary message;
- signing a digest;
- identifying a key without exposing it;
- reporting supported curves and signature schemes.

`sdk/signing/local/` and `sdk/signing/trezor/` implement that contract. A
Bitcoin or Ethereum wallet receives the signer contract from the application;
it does not construct, select, or depend on a concrete signer implementation.

Do not put files such as these inside a chain:

```text
chains/bitcoin/signing/local.rs
chains/bitcoin/signing/trezor.rs
```

That structure forces a chain to know concrete signer implementations and
couples every chain build to devices it may never use.

Chain-specific signing behavior is not a signer implementation. Bitcoin owns
Bitcoin sighash calculation and witness/script construction. Ethereum owns its
transaction signing payload and envelope construction. Those chain types ask
the generic signer to perform only the cryptographic operation.

### Use transaction builders, not a signing-plan abstraction

There are two initial reusable transaction families:

- `sdk/transactions/utxo/` for UTXO selection, outputs, fees, change, dust,
  and insufficient-funds checks;
- `sdk/transactions/account/` for common account-model fields such as sender,
  recipient, value, nonce, fees, and payload.

These are transaction models, not wallet models.

The reusable builders must be deterministic construction libraries. They do
not call RPC, read storage, broadcast transactions, or sign data. A chain
queries its own RPC/indexer, supplies the required values to the appropriate
builder, and converts the result into a chain-specific unsigned transaction.

The intended flow is:

```text
BitcoinTransactionBuilder
    └── uses UtxoTransactionBuilder
            └── produces selected inputs, outputs, fee, and change
                    └── Bitcoin produces UnsignedBitcoinTransaction
                            └── Bitcoin calculates sighashes
                                    └── generic Signer signs them
                                            └── Bitcoin produces SignedBitcoinTransaction
```

The equivalent account-model flow is:

```text
EthereumTransactionBuilder
    └── uses AccountTransactionBuilder
            └── Ethereum produces UnsignedEthereumTransaction
                    └── Ethereum calculates its signing payload
                            └── generic Signer signs it
                                    └── Ethereum produces SignedEthereumTransaction
```

Do not introduce `signing_plan.rs`. The meaningful states are builder,
unsigned transaction, and signed transaction.

UTXO and account models are starting abstractions, not a claim that every
chain fits them identically. Solana, for example, must not be forced into
Ethereum's nonce/value/gas model merely because both are sometimes described
as account-based.

### Indexing is generic; block interpretation is chain-specific

`sdk/indexing/` owns the reusable synchronization mechanism and structures:

- block positions and hashes;
- checkpoints and current synchronized height;
- synchronization ranges;
- atomic batches;
- rollback and reorganization behavior;
- a chain-source contract used by the sync engine.

It knows nothing about Bitcoin or Ethereum. A chain supplies the RPC/block
source and translates its blocks and transactions into the generic indexer
structures.

Synchronization begins at the earliest wallet or address creation height, not
at genesis unless required. The indexer advances to the current chain tip,
persists checkpoints, and must have an explicit reorganization strategy.

### Storage is backend-independent

`sdk/storage/` defines only the storage contract and the guarantees needed by
its consumers, including atomic updates where checkpoint and indexed data must
move together.

Storage knows nothing about Bitcoin, Ethereum, or concrete indexer rules. The
indexer depends on the storage abstraction; chains depend on the indexer and
may use the storage abstraction without exposing chain knowledge to it.

Do not add PostgreSQL, SQLite, RocksDB, memory, or other backend directories at
this design stage. A concrete backend will be selected and injected by an
application later. The architecture must not assume a backend before that is
an actual project concern.

### Packages are genuinely general-purpose

`packages/` contains libraries that could be transferred to an unrelated Rust
project without carrying blockchain or payment concepts with them.

Examples include:

- HTTP and Hyper wrappers;
- generic JSON-RPC client/server machinery;
- transports;
- logging and telemetry.

Concrete Bitcoin and Ethereum JSON-RPC methods and types remain in their chain
directories. `packages/json-rpc/` knows how JSON-RPC works; it does not know
which Bitcoin or Ethereum methods exist.

## Explicitly rejected directions

The following structures must not be reintroduced without changing this
architecture document first:

1. **A unified flat crate directory.** It hides architectural ownership and
   allows unrelated components to accumulate arbitrary dependencies.
2. **Crates directly at the repository root.** All Cargo packages belong under
   the `apps/`, `sdk/`, or `packages/` namespace.
3. **Horizontal catch-all crates such as `payment-ports` or
   `payment-domain`.** Bitcoin contracts and types must not be scattered across
   global buckets. Chain-specific code stays with its chain.
4. **Names such as `core`, `common`, or `utils`.** A name such as
   `signing-core` provides no ownership boundary and becomes a dumping ground.
   Name a package after its exact capability or contract.
5. **Bitcoin- or Ethereum-specific signer crates.** There should be no generic
   `signer-bitcoin` or `signer-ethereum` layer. The chain owns transaction
   interpretation; the signer owns cryptographic key operations.
6. **Concrete signer implementations inside chains.** Bitcoin and Ethereum
   depend only on the signer contract. The application selects local, Trezor,
   or another implementation.
7. **A `signing_plan` abstraction.** Use transaction builders and explicit
   unsigned/signed transaction states.
8. **Concrete storage backends in the architectural skeleton.** Storage is a
   contract for now; PostgreSQL or any other backend is not a current concern.
9. **Chain-aware generic packages.** HTTP and JSON-RPC wrappers are generic;
   concrete RPC methods and models stay inside the concrete chain.
10. **Reverse dependencies.** Signing, transaction families, indexing,
    storage, and generic packages must never import a concrete chain.
11. **Forcing every account-based chain into one concrete model.** Share only
    behavior that is truly common; keep protocol rules with the chain.
12. **Vague executable names.** Application folders describe real executable
    roles rather than acting as another architectural bucket.

## Review checklist

Before adding a package or dependency, answer all of these:

- Is the code chain-specific? If yes, is it physically inside that chain?
- Would deleting the chain directory remove every chain-specific reference?
- Can the signer package compile without any chain package?
- Can the transaction builder compile without RPC, storage, or signing?
- Can indexing and storage compile without Bitcoin or Ethereum?
- Can a package under `packages/` be reused without blockchain concepts?
- Does the new dependency point from a less generic layer to a more generic
  layer?
- Is the package name an exact responsibility rather than a catch-all?
- Is a concrete implementation being selected only in `apps/`?
- Is the abstraction required by current behavior rather than speculative?

If any answer is no, the ownership or dependency direction must be reconsidered
before implementation.
