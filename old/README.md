# Payment SDK

> [Target architecture](./ARCHITECTURE.md) defines the intended workspace
> structure, dependency direction, ownership rules, and explicitly rejected
> designs. The crate layout described below is the current experimental
> implementation, not the final architecture.

Payment SDK is a small, compilable Rust SDK skeleton for building payment
workflows across blockchain networks. It currently demonstrates Ethereum and
Bitcoin while using Alloy's signer, network-wallet, provider-filler, RPC-client,
and transport boundaries as conceptual guidance without copying Alloy's
protocol logic.

There is no real networking, serialization, cryptography, entropy, fee
calculation, nonce lookup, UTXO selection, transaction validation, or
broadcasting. Local credential generation uses mock process-local identifiers;
it does not create private keys.

## Current prototype workspace

```text
.
├── Cargo.toml
├── src/lib.rs                    facade and feature selection
├── crates/
│   ├── primitives/              Address<C>, TxHash<C>, Signature<C>
│   ├── signer/                  credential signing contracts
│   ├── signer-local/            generated mock local credentials
│   ├── network/                 transaction and wallet contracts
│   ├── signer-ethereum/         local credential → Ethereum adapter
│   ├── signer-bitcoin/          local credential → Bitcoin adapter
│   ├── network-ethereum/        Ethereum models, filler, and wallet
│   ├── network-bitcoin/         Bitcoin models, filler, and wallet
│   ├── provider/                ProviderBuilder and WalletFiller
│   ├── rpc-client/              RpcClient<T>
│   └── transport/               Transport and mock HTTP/WS adapters
├── examples/
│   ├── ethereum_http.rs
│   ├── ethereum_ws.rs
│   └── bitcoin_http.rs
└── tests/pipelines.rs
```

The previous `wallet` crate was removed. Shared transaction-signing and wallet
contracts now belong to `network`; concrete wallets remain in their chain
crates because an Ethereum signer registry and a Bitcoin signer registry own
different transaction types.

## Why `primitives` exists now

The network and chain-adapter crates share a small set of chain-tagged storage
types:

- `network` needs `Address<C>`, `Signature<C>`, and `TxHash<C>`;
- the Ethereum and Bitcoin signer adapters produce the same typed signatures
  consumed by their network envelopes.

`primitives` therefore owns their chain-tagged storage representation while
knowing no Ethereum, Bitcoin, signer, wallet, provider, RPC, or transport rules.
The protocol-independent `signer` crate owns its own `Digest`, `PublicKey`, and
`CredentialSignature` values and does not depend on `primitives`.

## Dependency direction

```text
signer-local ──► signer ◄── network ──► primitives
                    ▲          ▲
                    │          │
                    │          ├── network-ethereum
                    │          └── network-bitcoin
                    │
        signer-ethereum / signer-bitcoin

transport ◄── rpc-client ◄── provider
                              │
                              └── network

facade/examples/tests ──► focused crates
```

Direct Cargo edges:

```text
signer       -> no workspace crates
network      -> primitives + signer
signer-local -> signer
network-ethereum -> primitives + network
network-bitcoin  -> primitives + network
signer-ethereum  -> primitives + signer + network + network-ethereum
signer-bitcoin   -> primitives + signer + network + network-bitcoin
rpc-client   -> transport
provider     -> network + rpc-client + transport
facade       -> all focused crates
```

Lower-level crates never depend on the facade. `signer` has no dependency on
`network`, wallets, chain models, provider, RPC, or transport.

## Responsibility boundaries

### `Signer`

`signer` represents one protocol-independent credential capability:

```text
Signer
├── public_key()
├── sign_digest()
└── sign_message()
```

It does not derive network addresses, build transactions, route between
credentials, own wallets, or broadcast envelopes.

### `LocalSigner`

`signer-local` is the concrete credential adapter. `LocalSigner::generate()`
creates a unique mock credential identifier and mock public key for the current
process. It intentionally stores no real secret and uses no secure randomness.

```text
LocalSigner
├── CredentialId
├── PublicKey
└── Signer implementation
```

This is the correct owner for future local private-key generation. Hardware,
cloud, or remote credentials would live in separate adapter crates.

## Credential generation versus wallet construction

The two operations have different owners:

```text
LocalSigner::generate()       creates one mock credential
EthereumSigner::new(...)      adapts it to Ethereum
BitcoinSigner::new(...)       adapts it to Bitcoin
EthereumWallet::new(...)      stores/routes Ethereum signers
BitcoinWallet::new(...)       stores/routes Bitcoin signers
ProviderBuilder::wallet(...)  converts a signer or accepts a wallet
```

A wallet does not generate a credential. It receives one or more network
signers and routes transactions by address. `ProviderBuilder::wallet(signer)`
uses `IntoWallet` as a convenience for the common one-signer case.

### `TxSigner<C>` and `FullSigner<C>`

`network` owns transaction-facing signing:

```text
TxSigner<C>
├── address()
└── sign(unsigned_transaction)

FullSigner<C> = Signer<Signature = Signature<C>> + TxSigner<C>
```

`FullSigner` is a marker with a blanket implementation. It adds no methods.

### `NetworkWallet<C>`

`NetworkWallet` is the shared routing contract:

```text
NetworkWallet<C>
├── default_signer_address()
├── has_signer_for()
├── signer_addresses()
├── sign_transaction_from()
└── sign_request()
```

`EthereumWallet` and `BitcoinWallet` use an address-indexed
`HashMap<Address<C>, Arc<dyn TxSigner<C>>>`. Runtime polymorphism is deliberate
here: one wallet may route to heterogeneous local, hardware, cloud, or test
signers selected at runtime. The rest of the SDK uses compile-time generics.

### `IntoWallet<C>`

`IntoWallet` lets `ProviderBuilder::wallet(...)` accept either:

- a signer adapter that can construct its chain wallet; or
- an already configured wallet containing multiple signers.

The mock implementations live in separate chain adapter crates, where each
credential adapter owns its conversion without depending on the other chain:

```text
EthereumSigner.into_wallet() -> EthereumWallet
BitcoinSigner.into_wallet()  -> BitcoinWallet
```

### `WalletFiller<W>`

`WalletFiller` belongs to `provider`. It is the integration layer that:

1. receives the request after chain-specific fillers;
2. adds the wallet's default sender when `from` is missing;
3. asks the request to build its chain-specific unsigned transaction;
4. asks the wallet to route it to a `TxSigner`;
5. returns the signed chain envelope.

The provider then passes the envelope to `RpcClient<T>` and `Transport`.

## Transaction flows

Credential generation and implicit wallet construction:

```text
LocalSigner::generate()
        │
        ▼
EthereumSigner<LocalSigner>
        │
        ▼ IntoWallet<Ethereum>
EthereumWallet
        │
        ▼
ProviderBuilder::wallet(signer)
        │
        ▼
WalletFiller<EthereumWallet>
        │
        ▼
Provider<Ethereum, HttpTransport, WalletFiller<_>>
```

Transaction processing:

```text
EthereumTransactionRequest / BitcoinTransactionRequest
        │
        ▼
EthereumFiller / BitcoinFiller
        │
        ▼
Provider::send_transaction(request)
        │
        ▼
WalletFiller::fill_and_sign
        │
        ├── missing sender -> wallet default address
        ▼
TransactionRequest::build
        │
        ▼
EthereumUnsignedTx / BitcoinUnsignedTx
        │
        ▼
NetworkWallet::sign_transaction_from
        │
        ▼ address-indexed TxSigner
        │
        ▼
EthereumTxEnvelope / BitcoinTxEnvelope
        │
        ▼
Provider::send_envelope
        │
        ▼
RpcClient<T> -> Transport
```

## Examples

### Forming an Ethereum transaction request

The application chooses the recipient and value. It may omit `from`; the
provider's `WalletFiller` will use the wallet's default signer address.

```rust
fn create_transaction_request() -> EthereumTransactionRequest {
    let recipient = Address::<Ethereum>::new("0xreceiver");
    let value_wei = 1_000_u128;

    EthereumTransactionRequest::transfer(recipient, value_wei)
}

let request = create_transaction_request();

assert_eq!(request.from, None);
assert_eq!(request.to.as_str(), "0xreceiver");
assert_eq!(request.value_wei, 1_000);

let filled_request = EthereumFiller.fill(request);
assert_eq!(
    filled_request.steps(),
    [
        "ethereum transaction created",
        "nonce added",
        "ethereum fee added",
    ],
);
```

The stages are intentionally separate:

```text
recipient + value
        ↓ EthereumTransactionRequest::transfer
raw application request
        ↓ EthereumFiller::fill
request with mock nonce and fee
        ↓ Provider::send_transaction
sender + build + sign + send
```

Ethereum over HTTP with implicit wallet construction:

```rust
let credential = LocalSigner::generate()?;
let signer = EthereumSigner::new(credential).with_chain_id(1);
let signer_address = TxSigner::<Ethereum>::address(&signer).clone();

let provider = ProviderBuilder::<Ethereum>::new()
    .wallet(signer)
    .connect_http("http://localhost:8545");

assert_eq!(
    provider.wallet().default_signer_address(),
    &signer_address,
);

let request = create_transaction_request();
let filled_request = EthereumFiller.fill(request);
let result = provider.send_transaction(filled_request)?;

assert_eq!(result.message, "ethereum transaction sent over HTTP");
```

Explicit wallet construction is used for routing:

```rust
let default_credential = LocalSigner::generate()?;
let default_signer = EthereumSigner::new(default_credential);

let second_credential = LocalSigner::generate()?;
let second_signer = EthereumSigner::new(second_credential);

let mut wallet = EthereumWallet::new(default_signer);
wallet.register_signer(second_signer);

let provider = ProviderBuilder::<Ethereum>::new()
    .wallet(wallet)
    .connect_http("http://localhost:8545");
```

Ethereum read operation over WebSocket:

```rust
let provider = ProviderBuilder::<Ethereum>::new()
    .connect_ws("ws://localhost:8546")
    .await?;

let result = provider.get_balance(&Address::new("0xaccount"));
```

### Forming a Bitcoin transaction request

Bitcoin keeps its own request fields and filler behavior:

```rust
fn create_transaction_request() -> BitcoinTransactionRequest {
    let recipient = Address::<Bitcoin>::new("bc1qreceiver");
    let amount_sats = 25_000_u64;

    BitcoinTransactionRequest::transfer(recipient, amount_sats)
}

let request = create_transaction_request();

assert_eq!(request.from, None);
assert_eq!(request.to.as_str(), "bc1qreceiver");
assert_eq!(request.amount_sats, 25_000);

let filled_request = BitcoinFiller.fill(request);
assert_eq!(
    filled_request.steps(),
    [
        "bitcoin transaction created",
        "UTXO inputs selected",
        "bitcoin fee added",
    ],
);
```

Bitcoin uses the same general pipeline with Bitcoin-owned types:

```rust
let credential = LocalSigner::generate()?;
let signer = BitcoinSigner::new(credential, BitcoinNetwork::Mainnet);
let signer_address = TxSigner::<Bitcoin>::address(&signer).clone();

let provider = ProviderBuilder::<Bitcoin>::new()
    .wallet(signer)
    .connect_http("http://localhost:8332");

assert_eq!(
    provider.wallet().default_signer_address(),
    &signer_address,
);

let request = create_transaction_request();
let filled_request = BitcoinFiller.fill(request);
let result = provider.send_transaction(filled_request)?;
```

## What remains chain-specific

The workflow is shared; transaction semantics are not:

- requests and unsigned transactions;
- sender and recipient interpretation;
- account nonce/fee fields versus UTXO inputs and Bitcoin fees;
- transaction building;
- signer implementations;
- signed-envelope representation;
- transaction hashes and future RPC methods;
- future validation, encoding, error, and policy models.

`Chain` associates these types at compile time. An Ethereum provider cannot
accept a Bitcoin request, signer, wallet, unsigned transaction, or envelope.

## Intentional differences from Alloy

This mock SDK stays smaller:

- only synchronous mock signer traits are modeled;
- `LocalSigner::generate()` creates identifiers, not cryptographic keys;
- Ethereum and Bitcoin use separate mock signer adapter crates;
- chain wallets live in chain crates rather than a large shared network crate;
- fillers return fixed strings and typed mock values;
- no consensus, EIP, PSBT, keystore, hardware, cloud, or cryptographic code;
- no macro is needed for `IntoWallet`; two explicit implementations are clearer.

## Commands

```bash
cargo run --example ethereum_http
cargo run --example bitcoin_http
cargo run --example ethereum_ws --features ws

cargo test --workspace --all-features
cargo check --workspace --no-default-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --all-features --no-deps
```
