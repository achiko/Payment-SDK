# Architecture rules

## Dependency direction

```text
apps/*
  -> sdk/*
  -> packages/*

sdk/chains/{bitcoin,ethereum}
  -> sdk/chains/base
  -> sdk/indexing where block interpretation needs neutral facts
  -> packages/* for generic mechanisms

sdk/indexing/{http,rocksdb}
  -> sdk/indexing

packages/*
  -> packages/* only
```

Cargo arrows mean “depends on.” Dependencies flow from composition toward
reusable abstractions. `packages/*` never imports `sdk/*` or `apps/*`.
Concrete chain crates may consume protocol-neutral SDK contracts; generic SDK
crates may not import a concrete chain.

Applications may depend on any SDK/package needed for composition but may not
depend on another application. Every Cargo package remains under `apps/`,
`sdk/`, or `packages/`; do not introduce flat root crates or catch-all
`core`, `common`, or `utils` packages.

## Ownership

- `apps/indexer` is the runnable Bitcoin/Ethereum Indexer Service composition
  root. It selects one chain source/interpreter, one RocksDB repository, the
  generic HTTP router, and worker supervision.
- `apps/wallet` is the stateless Wallet Service composition root. Its binary
  can build configured Bitcoin and/or Ethereum wallets, RPC capabilities, and
  remote Indexer readers from explicit `WS_*` environment variables. With no
  chain configuration it remains live but truthfully not ready. It owns no
  storage or production custody policy.
- `apps/api` owns durable protocol-neutral payment orchestration and HTTP. Its
  binary composes RocksDB, remote indexing, Bitcoin/Ethereum native wallets,
  Ethereum ERC-20 wallets, reconciliation, mandatory bearer authentication,
  and one optional finite local deposit scope. TLS and production custody
  remain external.
- `sdk/chains/base` owns only approved protocol-neutral values and small
  capabilities: address, asset, chain/network, block reference, exact decimal,
  derivation, key pair, signing, transaction builder, signed transaction, and
  broadcaster.
- `sdk/chains/bitcoin` owns every Bitcoin address/network, RPC, UTXO, script,
  fee, transaction, sighash, witness, signing, and block-interpretation rule.
- `sdk/chains/ethereum` owns every Ethereum address/network, RPC, nonce, gas,
  EIP-1559/ERC-20, transaction, signing, receipt/log, and block-interpretation
  rule.
- `sdk/wallets` owns the protocol-neutral wallet capabilities, provider
  boundary, and `Wallets` composition collection.
- `sdk/indexing` owns chain-neutral watches, transaction facts, output facts,
  checkpoints, finality, replay, reorg, synchronization, and semantic
  repository contracts.
- `sdk/indexing/http` is the chain-neutral remote consumer implementation.
- `sdk/indexing/rocksdb` implements indexing repository contracts and keeps
  physical keys, records, and codecs private from chains and applications.
- `sdk/deposits` owns payment-only deposit, classification, accounting, job,
  reconciliation, and collection records. Indexing never imports these
  business meanings.
- `packages/crypto` owns transferable cryptographic mechanisms without chain,
  wallet, RPC, custody, or asset policy.
- `packages/http` and `packages/json-rpc` own generic client/server extensions
  and JSON-RPC framing, not chain methods or business response DTOs.
- `packages/storage` owns generic atomic storage mechanics; its RocksDB child
  supplies the engine without indexing semantics.

## Shared values and chain deletion

`base::Address` is opaque bytes. Concrete chains own parsing, display encoding,
checksum, and network validation. `Decimal` is the only generic monetary
representation; concrete chains alone convert to and from native integer units
such as satoshis or wei.

Persisted indexing identities are network-safe: `CanonicalAddress` and
`TransactionRef` each contain one complete `IndexScope` plus their canonical
text. Repositories, HTTP adapters, interpreters, wallets, and application
composition reject an identity whose chain or network differs from its request
scope. Concrete crates expose one canonical `CHAIN` key and use that same key in
`Chain` metadata and indexing scopes; ticker abbreviations are display metadata,
not persistence keys.

The chain deletion invariant is mandatory: deleting Bitcoin must remove every
Bitcoin-specific type while leaving base, Ethereum, wallets, indexing,
deposits, storage, HTTP, JSON-RPC, and crypto usable. The same rule applies to
every future chain. Generic crates must not contain chain names, tickers,
address encodings, RPC methods, or transaction DTOs.

## Signing and transactions

The required flow is:

```text
chain-native request
  -> chain-native builder
  -> chain computes payload/digest
  -> injected Signer signs cryptographic data
  -> chain verifies and assembles exact signed envelope
  -> persist SignedTransaction
  -> register transaction watch
  -> Broadcaster submits exact bytes
  -> Indexer observes confirmation and reorg state
```

`Signer` has one signing function and never receives a transaction, RPC client,
wallet policy, hardware capability, or user-interaction request. `KeyPair` is
currently the only implementation.

`TransactionBuilder` exposes transfer, versioned JSON snapshot, and prepare.
`BuilderCast::utxo` provides optional input/change controls without forcing a
UTXO model onto account chains. `SignedTransaction` is durable data—not an RPC
handle—and contains a version, chain-owned kind, canonical text ID, and exact
redacted-debug envelope. `Broadcaster` has one external-effect method.

Bitcoin owns native SegWit v0 P2WPKH and Taproot key-path input validation and
signing. Ethereum owns chain-ID/build-context validation, EIP-1559 envelopes,
and signer recovery. Never invent one universal native transaction model.

## Wallets

The wallet abstraction combines small capabilities:

- `Addresser`
- `AddressFormat`
- `AmountFormat`
- `BalanceReader`
- `TransactionFactory`
- `CollectionFactory`
- `Sweeper`
- `TransactionRestore`
- `HistoryReader`
- the one-method `Signer`

`Provider` creates one concrete wallet from secret bytes. `Wallets<K>` maps an
application-owned typed key to exactly one provider, rejects duplicate keys at
startup, and returns `Arc<dyn Wallet>`. Key generation, mnemonic policy, custody, RPC endpoints, and
authentication belong to application composition, not the wallet trait.

Wallets never wait for receipts. Balance/history use indexing facts, and
durable callers observe submitted transactions through `Indexer`.

`CollectionFactory::collector` optionally exposes selected-output collection.
The generic `Collector` accepts source wallets with exact selected outputs,
one destination, and returns `PreparedCollection` with `PreparedFee::Exact`.
`SelectedOutput` contains only an indexing `OutputId` plus its scale-zero
chain-native atomic `Decimal` reservation fence. Scripts and other spend
evidence remain in the concrete chain.

Bitcoin implements an exact multi-owner drain. It reloads every selected UTXO
from IX at one checkpoint, rejects duplicates, wrong ownership, and amount
drift, canonically orders inputs, and signs each input through its owning
wallet. Account-model draining instead uses the one-method `Sweeper`: Ethereum
native transfers `balance - maximum fee`, Ethereum tokens transfer their full
token balance after checking separate native fee capacity, and Bitcoin leaves
that capability unsupported. Account sweeps return `PreparedFee::Limit`; PS
persists that ceiling with the signed leg, while IX receipt facts determine
the factual fee and any residual balance.

Concrete chain RPC modules use one shared chain-local client for request
correlation and focused adapters for method families. Bitcoin separates node,
fee, and transaction operations; Ethereum separates account and transaction
operations. Generic HTTP and JSON-RPC transports own endpoint failover,
timeouts, authentication, and response limits. Applications inject those
adapters into wallets and workers instead of using a service-locator
connection.

## Indexing

The consumer surface is intentionally small:

- `Watcher`: watch and unwatch one selector lifecycle;
- `History`: transaction and address history reads;
- `Observer`: durable cursor-based revision events;
- `Indexer`: the marker combining those three traits;
- `OutputQuery`: a separate optional snapshot-consistent output read.

`Composer` routes consumer calls by exact `IndexScope`; duplicate scopes are a
conflict. The generic HTTP router uses
`/v1/scopes/{chain}/{network}/...`. Watch idempotency keys are always supplied
by callers. `indexing-http::Remote` supports bounded responses, timeouts,
retry, and multiple endpoints.

Concrete block interpreters produce semantic observation/output changes and
undo. They do not create RocksDB keys or encoded values. A canonical commit
atomically covers block effects, undo, checkpoint movement, observation
revisions, events, and outputs. UTXO transactions remain multiple independent
movements rather than a fake single sender/recipient amount.

Ordinary indexing APIs expose no physical schema or policy migration command.
The adapter rejects incompatible data; rebuild/restore is an explicit operator
operation outside the consumer traits.

## Payment orchestration

`apps/api::Payments` binds each concrete wallet to an exact indexing scope. Its
durable lifecycle is:

```text
Requested -> Prepared -> Watched -> Submitted -> Confirmed
```

Exact signed bytes are persisted before watch registration and broadcast.
Retries reuse those bytes and the same caller-owned watch identity.
`Payments::reconcile` consumes revision events and atomically commits payment
evidence with the per-scope cursor. Confirmation/finality advances state; a
reorg revision can return it to submitted.

`apps/api::Sweeps` owns account, token-with-gas, and UTXO collection execution
over a durable collection already created and reserved by `sdk/deposits`.
`DepositWallets` resolves each
durable deposit to an already composed abstract wallet without exposing secret
bytes or key locators; `GasWallet` resolves an application-owned native funding
wallet. Each ordered leg prepares once, records the exact signed
transaction and factual fee, registers the idempotent IX transaction watch,
and only then broadcasts. A lost response retries the stored exact bytes and
the same watch identity; it never signs again. A token sweep cannot be prepared
until IX confirms its gas leg. The application allocates the
scale-zero network fee proportionally by gross participant value using largest
remainder and deposit-ID tie-breaking, and durably records gross debit, master
credit, and allocated fee per deposit.

The wallet executable and Payment Service executable are implemented. The
Payment binary can compose address issuance, watches, observation,
balance/history, collection planning, and execution from an explicit finite
map of environment-referenced local keys. The planner accepts stable IDs only;
it derives policy, destination, amount, and spend resources from configured and
durable PS/IX state. One process supports one
optional Bitcoin-native, Ethereum-native, or ERC-20 deposit scope; ERC-20 may
select a same-scope native gas wallet. This is an application-owned in-process
resolver, not production custody. Multiple deposit scopes, TLS, HA, and
live-network readiness remain outside this composition.
