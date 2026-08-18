# Wallet library usage

Applications register concrete protocol providers once and then work only with
the protocol-neutral wallet contract:

```rust,ignore
enum WalletKey { Bitcoin, Ethereum }
let mut wallets = Wallets::new();
wallets.register(WalletKey::Bitcoin, bitcoin_provider)?;
wallets.register(WalletKey::Ethereum, ethereum_provider)?;
let wallet = wallets.new_wallet(&WalletKey::Bitcoin, secret).await?;

let balance = wallet.balance().await?;
let history = wallet.history(HistoryRequest::first(100)).await?;

let mut builder = wallet.transaction();
builder.transfer(destination, amount)?;
let signed = builder.prepare().await?;
persist(&signed)?;
wallet.broadcaster().broadcast(&signed).await?;
```

History is already suitable for wallet-facing business logic: every entry
contains status and reorg evidence, movements, network fee, timestamps, and
asset metadata. Amounts are exact display-unit decimals. Atomic satoshi, wei,
and token-unit values remain an indexing persistence detail and are converted
by the concrete wallet with the correct precision.

For a durable workflow, register a transaction watch before the final broadcast
and reconcile its state from `Observer`; `apps/api::Payments` implements that
ordering. A host application must provide concrete provider/RPC configuration,
secrets, and authentication.

The checked-in executable requires an inbound bearer secret and is live but
not ready without a wallet:

```bash
WS_HTTP_BIND=127.0.0.1:8082 \
WS_HTTP_BEARER_TOKEN='replace-with-secret-manager-value' \
mac cargo run --locked -p wallet-worker
```

Configure either complete wallet group to make it ready. These are the exact
Bitcoin variables:

```text
WS_BITCOIN_WALLET_ID
WS_BITCOIN_PRIVATE_KEY_HEX            # exactly 32 bytes; never logged
WS_BITCOIN_NETWORK                   # mainnet|testnet3|testnet4|signet|regtest
WS_BITCOIN_ADDRESS_FORMAT            # segwit_v0|taproot
WS_BITCOIN_RPC_URLS                   # ordered, comma-separated
WS_BITCOIN_RPC_AUTHORIZATION         # optional complete Authorization value
WS_BITCOIN_GENESIS_HASH
WS_BITCOIN_INDEXER_URLS               # ordered, comma-separated
WS_BITCOIN_INDEXER_TOKEN             # optional bearer token
WS_BITCOIN_TIMEOUT_SECONDS            # optional, default 15
WS_BITCOIN_FEE_TARGET_BLOCKS          # optional, default 6
WS_BITCOIN_MAX_FEE_RATE_SAT_PER_KVB   # optional, default 10000000
```

Bitcoin startup verifies the node network, genesis hash, synchronization, and
supported Bitcoin Core version before readiness. Spendable outputs and history
come from the configured Indexer Service; fee estimation, preflight, and
broadcast go to Bitcoin Core.

Ethereum uses:

```text
WS_ETHEREUM_WALLET_ID
WS_ETHEREUM_PRIVATE_KEY_HEX           # exactly 32 bytes; never logged
WS_ETHEREUM_NETWORK                  # mainnet|sepolia
WS_ETHEREUM_CHAIN_ID                 # 1 or 11155111, matching network
WS_ETHEREUM_RPC_URLS                  # ordered, comma-separated
WS_ETHEREUM_RPC_AUTHORIZATION        # optional complete Authorization value
WS_ETHEREUM_INDEXER_URLS              # ordered, comma-separated
WS_ETHEREUM_INDEXER_TOKEN            # optional bearer token
WS_ETHEREUM_TIMEOUT_SECONDS           # optional, default 15
```

Both groups may be present. Wallet IDs must be distinct because they are HTTP
route keys. `WS_HTTP_BIND` remains optional and defaults to
`127.0.0.1:8082`. Operators should prefer a secret manager that injects the
process environment and must protect process-environment access. The library
API remains available for hosts that compose another custody mechanism through
`Service::with`.

`WS_HTTP_TLS_TERMINATED_UPSTREAM=true` is required when `WS_HTTP_BIND` is not
loopback. It is an explicit deployment assertion that a trusted upstream
terminates TLS; it does not enable TLS inside this process.

All wallet routes require `Authorization: Bearer <WS_HTTP_BEARER_TOKEN>`;
liveness and readiness do not. Sending through HTTP is deliberately two-step:

```text
POST /v1/wallets/{wallet}/transactions
  { "destination": { ... }, "amount": "1.25" }
  -> complete SignedTransaction JSON

persist the exact response and register its transaction watch

PUT /v1/wallets/{wallet}/transactions/{transaction_id}
  <the exact SignedTransaction JSON>
  -> { "transaction_id": "..." }
```

On an ambiguous transport failure, retry the `PUT` with the persisted body.
Never call `POST` again: rebuilding can select different UTXOs or an account
nonce. WS owns no database, so business-command idempotency remains with the
caller; transaction-addressed exact-envelope resubmission is the only honest
idempotency guarantee at this boundary.

RPC and Indexer lists are attempted from left to right. Failover advances only
after retryable transport or HTTP failures; a protocol rejection is returned
by the endpoint that produced it. The singular `WS_BITCOIN_RPC_URL`,
`WS_BITCOIN_INDEXER_URL`, `WS_ETHEREUM_RPC_URL`, and
`WS_ETHEREUM_INDEXER_URL` names remain accepted for one-endpoint deployments.
Setting both forms for the same endpoint group is rejected so ordering is
never ambiguous.
