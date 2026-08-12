# Multi-chain wallet: primitives, interfaces, and architecture patterns

> Transcribed from two pages of handwritten notes and combined with an architectural pattern analysis. Ambiguities in the original design are retained and identified rather than silently resolved.

## Document metadata

- Language: Rust
- Chains: Bitcoin (BTC) and Ethereum (ETH)
- Layers: 3
- Traits: 6
- Source description: transcribed from two pages of handwritten notes

## Design premise

One idea runs through the design:

> Data is shared across chains, while behavior sits behind traits.

Bitcoin and Ethereum diverge at their builder, signer, broadcaster, and chain-specific indexing implementations. The intention is that they do not diverge in the common types exchanged across those boundaries.

## Transaction lifecycle

The traits and components participate in the following order:

1. **`Wallet`** holds an address and keypair.
2. **`TxBuilder`** turns inputs and a destination into an unsigned transaction.
3. **`Signer`** signs the transaction. `LocalSigner` wraps a keypair.
4. **`TxBroadcaster`** submits the signed transaction and waits for confirmations.
5. **`Transactioner`** represents the indexing/read-back side and normalizes chain data into a shared transaction.

```text
Wallet -> TxBuilder -> Signer -> TxBroadcaster -> Transactioner
```

## Layer 1 — Primitives

This layer contains data types with little or no behavior. Newtypes wrap primitive values so, for example, an address cannot accidentally be supplied where a transaction ID is required.

### `Address`

```rust
pub struct Address(String);
```

- Pattern: newtype
- Underlying representation: `String`
- The notes originally used separate `BitcoinAddr` and `EthAddr` types, then collapsed them into one shared type.

### `TransactionId`

```rust
pub struct TransactionId(String);
```

- Pattern: newtype
- Underlying representation: `String`

### `NetworkId`

```rust
pub struct NetworkId(u32);
```

- Pattern: newtype
- Underlying representation: integer, represented as `u32` in the Rust skeleton

### `Chain`

```rust
pub struct Chain {
    network_id: NetworkId,
    name: String,
}
```

Example from the notes:

```rust
Chain {
    network_id: NetworkId(0),
    name: "btc".to_owned(),
}
```

The source describes Bitcoin mainnet conceptually as `Chain { network_id: 0, name: "btc" }`.

### `Transaction`

```rust
pub struct Transaction {
    to: Address,
    amount: Decimal,
    id: TransactionId,
}
```

The `from` field is deliberately absent. It was crossed out because a UTXO transaction does not necessarily have one sender. Ethereum-specific `from()` behavior remains on `EthTx` instead.

### `Keypair`

```rust
pub struct Keypair {
    address: Address,
    key: String,
}
```

Associated operation:

```rust
fn address(&self) -> Address;
```

## Layer 2 — Traits

This layer defines the shared behavioral interfaces. It corresponds to the middle and right columns of the first handwritten page and the top of the second page.

The original notes place a star beside `TxBuilder::sign()`, marking it as the builder's core method.

### `Addresser`

```rust
pub trait Addresser {
    fn address(&self) -> Address;
}
```

### `Transactioner`

```rust
pub trait Transactioner {
    fn transaction(&self) -> Transaction;
}
```

This was first called `TransactionUtxo`, then renamed. The indexer-facing abstraction normalizes both chain-specific transaction shapes into the shared `Transaction` type; consequently, the shared trait is not named after Bitcoin's UTXO model.

### `Wallet`

```rust
pub trait Wallet {
    fn address(&self) -> Address;
    fn keypair(&self) -> Keypair;
}
```

### `Signer`

```rust
pub trait Signer {
    fn sign(&self, payload: &[u8]) -> Signature;
}
```

The concrete local implementation is constructed from a keypair:

```rust
LocalSigner::new(keypair)
```

The qualifier **local** is important: it leaves room for remote, hardware-wallet, or HSM-backed signers behind the same trait.

### `TxBuilder`

The handwritten version includes these operations:

```rust
fn destination(&self) -> Address;
fn sign(/* ... */); // ★ core method
fn transaction_id(&self) -> Option<TransactionId>;
fn broadcast(/* ... */) -> Broadcaster;
```

The normalized Rust skeleton later narrows the trait to:

```rust
pub trait TxBuilder {
    fn destination(&self) -> Address;
    fn sign(self) -> SignedTx;
    fn transaction_id(&self) -> Option<TransactionId>;
}
```

The design is intended to work with Bitcoin UTXO inputs or an Ethereum wallet/signer. `transaction_id()` replaces a crossed-out `hash()` method and returns a domain type. Its `Option` expresses the original idea that a transaction has no ID before it is signed.

The design remains unresolved about whether broadcasting belongs on the builder. `broadcast()` appears on both `TxBuilder` and the dedicated `TxBroadcaster` abstraction and is partly crossed out on the builder. The cleaner boundary proposed in the original document is to end the builder at `sign()`.

### `TxBroadcaster`

```rust
pub trait TxBroadcaster {
    fn broadcast(&self, tx: SignedTx) -> TransactionId;
    fn wait(&self, id: &TransactionId, confirmations: u8);
}
```

The handwritten notation `wait(0,1)` is interpreted as:

- `0` confirmations: mempool acceptance
- `1` confirmation: first on-chain confirmation

## Layer 3 — Per-chain implementations

The intended chain-specific difference lives in this layer. Bitcoin builds a transaction from a list of UTXO inputs, whereas Ethereum builds a transaction around one account signer.

### Bitcoin — UTXO model

#### `BtcTxBuilder`

```rust
BtcTxBuilder::new(inputs, destination)
```

Operations shown in the notes:

```rust
fn sign(/* ... */);
fn broadcast(/* ... */);
```

The later skeleton expresses its constructor as:

```rust
impl BtcTxBuilder {
    pub fn new(inputs: Vec<BtcInput>, destination: Address) -> Self;
}
```

#### `BtcInput`

```rust
pub struct BtcInput {
    signer: Box<dyn Signer>,
    index: u8,
}
```

Inputs are stored as a collection. Each input is signed separately, so its signer belongs to the input rather than to the builder as a whole.

#### Output and destination

The notes include:

```text
output index: u8
destination: Address
```

An earlier version listed `output`, `destination`, and `keypair` as loose fields. That version was crossed out and replaced by the structured input representation above.

#### `BtcTxBroadcaster`

```rust
BtcTxBroadcaster::new(JsonRpc(/* ... */))
```

Operation:

```rust
fn broadcast(/* TransactionId in the notes */);
```

The transport is injected. The broadcaster receives a JSON-RPC client instead of constructing one internally.

The normalized skeleton later changes the broadcaster contract to accept a `SignedTx` and return its `TransactionId`:

```rust
fn broadcast(&self, tx: SignedTx) -> TransactionId;
```

This is one of the differences between the handwritten interface and its later normalization.

#### `BitcoinTx` indexer representation

```rust
fn transaction(&self) -> Transaction;
fn output(&self) -> int;
```

`transaction()` maps the Bitcoin-specific representation to the shared `Transaction`. `output()` retains information that is specific to the Bitcoin/UTXO side.

### Ethereum — account model

#### `EthTxBuilder`

```rust
EthTxBuilder::new(signer, destination)
```

Operation shown in the notes:

```rust
fn sign(/* ... */);
```

The later skeleton expresses its constructor as:

```rust
impl EthTxBuilder {
    pub fn new(signer: Box<dyn Signer>, destination: Address) -> Self;
}
```

Ethereum uses one signer for the whole transaction. It has no Bitcoin-style input array, and the notes do not list `broadcast()` on this builder.

#### Builder inputs

```text
signer: Signer
destination: Address
```

`keypair` was crossed out and replaced by `signer`, which is the same correction made on the Bitcoin side. Builders therefore depend on signing behavior rather than directly holding a private key.

#### `EthTx` indexer representation

```rust
fn transaction(&self) -> Transaction;
fn from(&self) -> Address;
```

`transaction()` maps the Ethereum-specific representation to the shared `Transaction`. `from()` survives only on the Ethereum representation after being removed from the common transaction type.

## Consolidated Rust skeleton from the source

The following is the skeleton shown in the HTML document. It fills in return types where the handwritten pages left them implicit. It is an architectural sketch: placeholder types, omitted bodies, and declaration-only inherent methods would still need implementation before this could be compiled as a complete Rust module.

```rust
// ---- primitives ----
pub struct Address(String);
pub struct TransactionId(String);
pub struct NetworkId(u32);

pub struct Chain {
    network_id: NetworkId,
    name: String,
}

pub struct Transaction {
    to: Address,
    amount: Decimal,
    id: TransactionId,
}

pub struct Keypair {
    address: Address,
    key: String,
}

// ---- interfaces ----
pub trait Addresser {
    fn address(&self) -> Address;
}

pub trait Transactioner {
    fn transaction(&self) -> Transaction;
}

pub trait Wallet {
    fn address(&self) -> Address;
    fn keypair(&self) -> Keypair;
}

pub trait Signer {
    fn sign(&self, payload: &[u8]) -> Signature;
}

pub trait TxBuilder {
    fn destination(&self) -> Address;
    fn sign(self) -> SignedTx;                       // ★ core method
    fn transaction_id(&self) -> Option<TransactionId>; // None until signed
}

pub trait TxBroadcaster {
    fn broadcast(&self, tx: SignedTx) -> TransactionId;
    fn wait(&self, id: &TransactionId, confirmations: u8);
}

// ---- implementations ----
pub struct BtcInput {
    signer: Box<dyn Signer>,
    index: u8,
}

impl BtcTxBuilder {
    pub fn new(inputs: Vec<BtcInput>, destination: Address) -> Self;
}

impl EthTxBuilder {
    pub fn new(signer: Box<dyn Signer>, destination: Address) -> Self;
}

impl LocalSigner {
    pub fn new(keypair: Keypair) -> Self;
}

impl BtcTxBroadcaster {
    pub fn new(rpc: JsonRpc) -> Self;
}
```

## Decisions crossed out in the source

These decisions were visibly rejected in the handwritten notes and retained in the HTML so they would not be repeatedly reconsidered.

| Dropped | Replaced by | Reason |
|---|---|---|
| `from: Address` in the common transaction | No common replacement | A UTXO transaction may have multiple senders. `from()` remains only on `EthTx`. |
| `TransactionUtxo` | `Transactioner` | A shared trait should not be named after one chain's transaction model. |
| `keypair` in builders | `signer` | Builders should not hold private keys; they should hold or reference something capable of signing. |
| `hash()` | `transaction_id()` | The replacement returns a domain type and can use `Option` because an unsigned transaction has no ID in the original model. |
| `BitcoinAddr` / `EthAddr` | `Address` | The notes choose one shared type and delegate validation to the chain layer. |
| `btc` qualifier on `TxBroadcaster` | No qualifier | The trait was briefly Bitcoin-specific and was then generalized. |

## Open questions retained from the source

1. **Crate layout.** The sketch at the bottom left of page two—headed *Package* and something resembling *chains / crates*, with boxes, arrows, and a final *Before*—is not legible in either scan. Based on the stated layers, the suggested split is:

   ```text
   core        # primitives and traits
   chain-btc   # Bitcoin implementations
   chain-eth   # Ethereum implementations
   indexer     # chain read-back and normalization
   ```

2. **`broadcast()` appears twice.** It appears once on `TxBuilder` and again as the entire responsibility of `TxBroadcaster`; it is partly crossed out on the builder. The proposed cleaner separation is for the builder to end at `sign()`.

3. **Who owns `Chain`?** The type is defined but never connected to the rest of the design. `Wallet` and the builder are both possible owners.

4. **`IndexEvent`.** This appears only as a heading above the BTC/ETH fork. The event type itself is missing.

5. **`Decimal` for amounts.** Both Bitcoin and Ethereum settle in integer base units—satoshis and wei. A decimal representation normally belongs at the display boundary rather than in a settlement primitive.

6. **Asynchronous network operations.** `broadcast()` and `wait()` perform network I/O, so the traits will likely need asynchronous methods or methods returning futures.

---

# Architecture and design-pattern analysis

## Overall classification

The dominant architectural style is **Hexagonal Architecture (Ports and Adapters)** combined with explicit **Layered Architecture**.

It is also **Clean Architecture–inspired**, but the document does not yet define a complete Clean Architecture. In particular, it does not show an application/use-case layer or fully state and enforce the dependency rule between all modules.

## Pattern mapping

| Pattern | Where it appears | Interpretation |
|---|---|---|
| **Layered Architecture** | Primitives → traits → per-chain implementations | The design separates shared data, behavioral contracts, and concrete chain code into three conceptual layers. |
| **Hexagonal Architecture / Ports and Adapters** | `Signer`, `TxBuilder`, `TxBroadcaster`, and `Transactioner` traits; BTC, ETH, local signer, and JSON-RPC implementations | Traits act as ports. Chain-specific and infrastructure-specific implementations act as adapters. |
| **Dependency Inversion Principle** | Core-facing code refers to traits such as `Signer`; concrete dependencies such as `JsonRpc` are passed into implementations | High-level behavior is intended to depend on abstractions rather than constructing concrete services internally. |
| **Strategy** | Multiple signer, builder, and broadcaster implementations can satisfy the same trait | Chain-specific algorithms or custody mechanisms can be selected behind a common behavioral contract. |
| **Adapter / Canonical Data Model** | `BitcoinTx::transaction()` and `EthTx::transaction()` return the common `Transaction` | Different native transaction representations are converted into one shared shape for consumers. |
| **Builder** | `BtcTxBuilder` and `EthTxBuilder` | Each builder collects chain-specific construction inputs and produces a signed transaction through `sign()`. |
| **Newtype** | `Address`, `TransactionId`, and `NetworkId` | Wrapping primitives gives domain values distinct Rust types and prevents some categories of argument mix-up. |
| **Processing Pipeline** | `Wallet -> TxBuilder -> Signer -> TxBroadcaster -> Transactioner` | Transaction handling is presented as a sequence of focused stages. |
| **Partial Typestate** | `sign(self) -> SignedTx`, together with `Option<TransactionId>` | Consuming the unsigned builder to produce `SignedTx` suggests a state transition. The model is only partial because other transaction state is still expressed at runtime with `Option`. |
| **Dependency Injection** | `BtcTxBuilder::new(inputs, destination)`, `EthTxBuilder::new(signer, destination)`, and `BtcTxBroadcaster::new(rpc)` | Signers, inputs, destinations, and transport clients are supplied from outside. |

## Why Ports and Adapters is the best primary label

The central boundary in this design is not merely a class hierarchy. It separates stable, chain-neutral contracts from variable chain or infrastructure code:

```text
                 core ports
        Signer / TxBuilder / TxBroadcaster
                     ^
                     |
        concrete adapters and strategies
     BTC / ETH / LocalSigner / JSON-RPC client
```

For that reason, **Ports and Adapters plus Strategy** describes the design more precisely than the classic **Bridge** pattern. The structure may resemble Bridge because abstractions and implementations vary independently, but the stronger architectural intent is dependency isolation around the core.

## Important qualifications and design risks

### A shared `Address` is only partly type-safe

`Address(String)` prevents an address from being confused with a `TransactionId`, but a single shared address type does not prevent a Bitcoin address from being supplied to an Ethereum builder.

Possible stronger models include:

```rust
struct Address<C> {
    value: String,
    _chain: PhantomData<C>,
}
```

or an explicit enum:

```rust
enum Address {
    Bitcoin(BitcoinAddress),
    Ethereum(EthereumAddress),
}
```

Alternatively, the current shared representation can remain if every chain adapter performs strict validation at its boundary, but then cross-chain correctness is enforced at runtime rather than by the type system.

### `Wallet::keypair()` weakens the custody boundary

Returning a `Keypair` exposes private-key material to callers and conflicts with the otherwise strong abstraction introduced by `Signer`.

A safer boundary would expose signing capability without returning the key:

```rust
pub trait Wallet {
    fn address(&self) -> Address;
    fn signer(&self) -> &dyn Signer;
}
```

The exact API may differ, but the architectural principle is that local, remote, hardware, and HSM custody should be usable without extracting private keys.

### The transaction-state model is incomplete

`sign(self) -> SignedTx` is a strong type-level transition, while `transaction_id() -> Option<TransactionId>` represents state dynamically. Depending on the target chains and broadcast semantics, an ID may be derivable after signing, after serialization, or only as a result of submission.

The design should define explicit states and ownership, for example:

```text
UnsignedTx -> SignedTx -> SubmittedTx -> ConfirmedTx
```

This would clarify which state owns the transaction ID and which operations are legal at each stage.

### Builder and broadcaster responsibilities overlap

Keeping `broadcast()` on both the builder and `TxBroadcaster` mixes transaction construction with network transport. The source already leans toward resolving this by ending the builder at `sign()` and giving submission and confirmation waiting exclusively to `TxBroadcaster`.

### The common `Transaction` may be too lossy

The shared model contains only destination, amount, and ID. Bitcoin can have several inputs and outputs, while Ethereum has sender, nonce, gas, and potentially contract-call data. A deliberately small normalized projection is useful, but it should be named and documented as a projection if it is not intended to represent the full native transaction.

### Amounts should use integer base units

Using `Decimal` in the core risks ambiguous rounding and precision rules. Strong amount types—potentially parameterized by asset or chain—would better reflect settlement values:

```text
Bitcoin: satoshis
Ethereum: wei
Tokens: token-defined integer base units
```

Decimal formatting can then be applied at API or presentation boundaries.

## Concise conclusion

The design is best summarized as:

> A layered, ports-and-adapters multi-chain wallet design that uses Rust traits as ports, chain-specific implementations as adapters and strategies, newtypes for domain safety, dependency injection at construction boundaries, and a partially type-enforced transaction pipeline.

Its strongest ideas are the separation of signing from keys, injection of transport dependencies, normalization of chain read models, and isolation of BTC/ETH behavior. Its largest unresolved issues are chain-safe addressing, private-key exposure through `Wallet`, duplicated broadcasting responsibility, incomplete transaction states, amount representation, and the absence of a fully defined application/use-case layer.

