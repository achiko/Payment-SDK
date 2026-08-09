# Ethereum Wallet Service HTTP API

`apps/wallet` is the stateless Wallet Service (WS) composition root. It selects
one configured Ethereum chain, the chain-owned HTTP JSON-RPC adapter, and the
chain-independent remote custody client. It does not open a database and does
not own users, deposits, watches, jobs, reservations, retries, accounting, or
multi-leg collection sequencing.

For direct in-process Rust integration with every asynchronous
`WalletService` operation, use the step-by-step
[`Wallet Service Rust library guide`](./WALLET_SERVICE_USAGE.md).

## Runtime safety

- All operation routes require the configured bearer token.
- `/health/live` and `/health/ready` are unauthenticated and detail-free.
- Readiness is enabled only after the RPC reports the configured chain ID and
  custody reports secp256k1 ECDSA digest-signing capability and availability.
- Readiness is disabled before SIGINT/SIGTERM graceful drain.
- Plain HTTP RPC and custody endpoints are accepted only on loopback. External
  endpoints require HTTPS. A non-loopback WS listener requires an explicit
  trusted-upstream TLS assertion.
- Request bodies, RPC/custody responses, timeouts, retries, gas, and fee values
  are bounded by configuration.
- RPC URLs, RPC header values, bearer tokens, custody URLs, signed envelopes,
  and custody credentials are redacted from `Debug` and logs.

## Encoding rules

The JSON contract is strict: unknown fields are rejected. Ethereum addresses
and transaction IDs use lowercase `0x`-prefixed hexadecimal. Amounts use
canonical unsigned U256 decimal strings. Signed envelopes use lowercase
`0x`-prefixed hexadecimal. Key locators remain opaque and use either:

```json
{"kind":"identifier","value":"opaque-backend-handle"}
```

or:

```json
{"kind":"derivation_path","children":[{"index":7,"hardened":true}]}
```

Assets use `{"kind":"native"}` or
`{"kind":"erc20","token":"0x..."}`.

## Routes

All routes below use `POST` and `application/json`.

| Path | Semantics |
|---|---|
| `/v1/ethereum/addresses` | Provision one key and return only its canonical address and opaque key locator. Request fields are `operation_id`, `asset`, and `key_purpose`. No birthday is returned; PS owns birthday/watch orchestration. |
| `/v1/ethereum/balances` | Read factual native or ERC-20 confirmed, pending, and spendable amounts. |
| `/v1/ethereum/transfers/native/sign` | Build and sign one EIP-1559 native transfer without broadcast. |
| `/v1/ethereum/transfers/erc20/sign` | Build canonical ERC-20 `transfer` calldata and sign without broadcast. |
| `/v1/ethereum/collections/requirements` | Report factual native-gas prerequisites for one native or token collection request. |
| `/v1/ethereum/collections/native/sign` | Prepare and sign one native sweep without broadcast; returns attribution. |
| `/v1/ethereum/collections/erc20/sign` | Prepare and sign one token sweep without broadcast; returns attribution. |
| `/v1/ethereum/transactions/broadcast` | Accept `expected_transaction_id` and the exact `signed_envelope`, verify their Keccak-256 relationship, broadcast unchanged bytes, and require the provider to return the same ID. |
| `/v1/ethereum/receipts` | Read the current factual receipt for one canonical transaction ID. |

Signing responses contain `transaction_id` and `signed_envelope`. Collection
signing also returns `attribution`. These calls do not broadcast. PS can persist
the exact envelope before calling the broadcast route, making response loss and
retry recovery explicit without giving WS durable workflow ownership.

Error responses use stable JSON fields: `code`, `message`, `retryable`, and an
opaque `request_id`. Transport, RPC, and custody internals are sanitized.

## Serve configuration

Run `wallet-worker serve --help` for the complete CLI/environment mapping. The
required inputs select:

- Ethereum chain ID, RPC URL, optional repeatable `name=value` RPC headers,
  request timeout/retry/response bounds, gas margin, and gas/fee ceilings;
- remote custody URL, bearer credential, timeouts, retries, and response bound;
- WS bind address, WS bearer credential, upstream TLS assertion, request-body
  bound, and graceful-shutdown deadline.

No database path exists by design.
