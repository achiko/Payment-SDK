# Contract walkthrough

The repository is intentionally split between compile-time chain typing and
runtime implementation selection.

## Chain typing

[`Chain`](../sdk/chains/contract/src/chain.rs) is an associated-type map. It
preserves each chain's native asset, address, amount, transaction, and receipt
types instead of forcing them into an incomplete universal transaction.

Small capabilities operate on that map:

- [`DepositAddressGenerator`](../sdk/chains/contract/src/wallet.rs) combines
  chain-native address derivation with an injected key provisioner;
- [`BalanceReader`](../sdk/chains/contract/src/wallet.rs) reads an address/asset
  balance without retaining state;
- [`TransferBuilder`](../sdk/chains/contract/src/transaction.rs) prepares a
  chain-native unsigned transaction;
- [`TransactionSigner`](../sdk/chains/contract/src/transaction.rs) combines
  that transaction with an injected generic signer;
- [`Broadcaster`](../sdk/chains/contract/src/transaction.rs) submits the signed
  chain envelope;
- [`TransactionReader`](../sdk/chains/contract/src/transaction.rs) returns its
  current receipt/status;
- [`Collector`](../sdk/chains/contract/src/transaction.rs) reports prerequisites
  and executes one stateless, chain-owned collection attempt.

[`WalletAdapter`](../sdk/chains/contract/src/wallet.rs) is an optional facade
combining those capabilities at the WS boundary, and `WalletFactory<C>` selects
an adapter for a concrete chain asset. The individual capabilities remain the
unit of code ownership and testing.

There is no single god `ChainService` trait. An application can require only
the capabilities used by a given workflow.

## Signing

[`Signer`](../sdk/signing/contract/src/signer.rs) is object-safe. The
application can select a local, Trezor, HSM, KMS, remote, or test signer at
runtime and inject `&dyn Signer` into a chain.

Key allocation and signing are deliberately separate. A
[`KeyProvisioner`](../sdk/signing/contract/src/signer.rs) returns a public key
and opaque `KeyLocator`; a [`Signer`](../sdk/signing/contract/src/signer.rs)
authorizes operations for that locator. The signer receives:

- a backend-neutral [`KeyLocator`](../sdk/signing/contract/src/key.rs);
- either arbitrary message bytes or an already computed digest;
- an explicit signature scheme;
- an explicit signature encoding;
- optional public curve-level tweak material for operations such as Taproot
  key-path signing; and
- whether user interaction is allowed or required.

The signer does not receive a Bitcoin or Ethereum transaction. The concrete
chain computes its protocol signing payload and inserts the returned signature
into the chain-native transaction.

[`signer-local`](../sdk/signing/local/src/lib.rs) supplies a deliberately
ephemeral `KeyProvisioner + Signer` for tests and local experiments. It retains
random secp256k1 private keys only in process memory, exposes opaque locators
and public keys, produces recoverable/compact/DER ECDSA signatures, and
supports raw Schnorr signing with signer-internal public scalar tweaks. It does
not export private keys or provide HD derivation, persistence, or production
custody. `Arc<T>` delegates both signer contracts so one custody instance can
provision and later sign for the same locator.

Current flow:

```text
chain transaction builder
    -> chain-native unsigned transaction
    -> chain computes message/digest
    -> signer signs cryptographic payload
    -> chain validates and inserts signature
    -> chain-native signed transaction
    -> broadcaster
```

Hardware-wallet native transaction protocols are an unresolved exception; see
[`REQUIREMENTS.md`](./REQUIREMENTS.md#hardware-signing-boundary).

## Wallet Service implementation

[`wallet_worker::WalletService`](../apps/wallet/src/lib.rs) is the stateless
application facade. It selects the per-asset adapter, shares injected custody,
and exposes address generation, balances, unsigned construction, signing,
broadcast, receipt reads, requirements, and one-shot collection. It has no
storage dependency and does not own PS/IX workflow state.

[`BitcoinWallet`](../sdk/chains/bitcoin/src/wallet.rs) implements the complete
capability set for native Bitcoin over an injected `BitcoinRpc`. Its native
codec performs deterministic largest-first selection, dust/change/fee checks,
RBF inputs, P2WPKH signing, Taproot key-path signing, and batched collection
attribution.

[`EthereumWallet`](../sdk/chains/ethereum/src/wallet.rs) implements the complete
capability set for native ETH and ERC-20 assets over an injected `EthereumRpc`.
Its codec produces EIP-1559 envelopes and verifies the recovered sender;
collection calculates native maximum gas cost and encodes ERC-20 `transfer`.

The current phase intentionally does not select an HTTP framework or wire
protocol. A deployment may adapt this semantic facade in-process, over HTTP,
or through a queue without changing service ownership.

## PS deposit and accounting contracts

[`deposits`](../sdk/deposits/src/lib.rs) is the only SDK package allowed to add
payment semantics to IX facts:

- [`DepositStore`](../sdk/deposits/src/store.rs) persists deposit lifecycle,
  including the recoverable `AwaitingWatch` state;
- [`ObservationEventLog`](../sdk/deposits/src/event_log.rs) is PS's append-only,
  idempotent mirror of relevant IX event revisions;
- [`ObservationClassifier`](../sdk/deposits/src/classification.rs) consults PS
  records to identify incoming, collection, and gas-funding movements;
- [`DepositLedger`](../sdk/deposits/src/accounting.rs) appends an immutable row
  containing absolute `received`, `confirmed`, `balance`, `collected`, and
  `accounted` values after each relevant transition; `AccountingCommand` alone
  may change `accounted`;
- [`CollectionStore`](../sdk/deposits/src/store.rs) persists account, UTXO batch,
  and token-with-gas collection legs across restarts.

IX and concrete chain crates do not depend on `deposits`.

## IX public contracts

Internal block synchronization and the externally useful observation API are
separate:

- [`IndexingWorker`](../sdk/indexing/src/service.rs) advances one canonical
  chain/network checkpoint;
- [`ObservationRegistry`](../sdk/indexing/src/service.rs) registers address or
  transaction watches;
- [`ObservationQuery`](../sdk/indexing/src/service.rs) answers `tx(txid)` and
  `txs(address)` semantics;
- [`ObservationEventSource`](../sdk/indexing/src/service.rs) exposes a durable,
  cursor-based at-least-once event feed;
- [`ObservedTransaction`](../sdk/indexing/src/observation.rs) contains only
  chain facts, including multiple movements, fee, status, and revision.

The same contracts can be adapted to in-process calls, HTTP, queues, polling,
or WebSockets without changing their semantics. The first persistent
implementation uses one composite `IndexRepository`: block/undo state,
checkpoint movement, current observations, immutable revisions, confirmation
advancement, and feed rows are one atomic command boundary. Interpreters return
`ObservationDraft` values and never allocate repository revisions or cursors.

For Ethereum v1, HTTP polling is canonical and optional `newHeads` only wakes
reconciliation. The public status phase is explicit, depth 12 is confirmation
policy rather than consensus finality, and rollback retention is 50 bundles
plus one predecessor anchor. See [`INDEXER_SERVICE.md`](./INDEXER_SERVICE.md).

## UTXO construction

[`transaction-utxo`](../sdk/transactions/utxo/src/lib.rs) accepts available
UTXOs, recipients, a change destination, fee rate, and minimum change. Its
output contains selected inputs, recipients, optional change, and fee.

It must remain pure:

- no RPC calls;
- no storage reads;
- no signing;
- no Bitcoin scripts, consensus encoding, or sighashes.

[`BitcoinTransactionBuilder`](../sdk/chains/bitcoin/src/transaction/builder.rs)
uses those reusable algorithms and adds Bitcoin rules. Bitcoin then owns
sighashes, scripts, witnesses, and consensus bytes.

## Account construction

[`transaction-account`](../sdk/transactions/account/src/lib.rs) currently
models only transfer data plus a supplied nonce/fee context. RPC population is
chain-owned and occurs before the pure builder.

[`EthereumTransactionBuilder`](../sdk/chains/ethereum/src/transaction/builder.rs)
adds chain ID, gas, EIP-1559 fees, envelope selection, and Ethereum encoding.
Solana must receive a separate chain-native builder rather than being forced
through this Ethereum-shaped contract.

## Storage

[`Storage`](../sdk/storage/src/storage.rs) provides a small atomic key/value
contract with version preconditions, prefix scans, and atomic write batches.
This allows optimistic concurrency and makes it possible to persist a block's
events, undo data, and checkpoint together.

The Ethereum v1 implementation validates this API with a serialized RocksDB
adapter. Conditional reads and one synchronous WAL-backed write batch form a
logical commit. Semantic repositories still own record schemas, idempotency,
cursor allocation, migrations, and rebuild behavior; backend independence does
not expose raw storage operations to application policy.
