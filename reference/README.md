# Local research repositories

The child directories in `reference/` are shallow Git checkouts
and are ignored by the parent repository. They are not workspace members,
vendored dependencies, or licensed as part of this project.

| Directory | Repository | Inspected revision | Purpose |
|---|---|---:|---|
| `alloy/` | <https://github.com/alloy-rs/alloy> | `9bdfb5c72ebf` | network associated types, builders, wallets, signers, providers |
| `ethers-js/` | <https://github.com/ethers-io/ethers.js> | `3ea4c226dd0b` | signer/provider/population comparison |
| `rust-bitcoin/` | <https://github.com/rust-bitcoin/rust-bitcoin> | `ddae76341ceb` | Bitcoin transaction, PSBT, sighash, script types |
| `bdk/` | <https://github.com/bitcoindevkit/bdk> | `337e9d68414c` | wallet sync requests, checkpoints, change sets, tx graph |
| `blockbook/` | <https://github.com/trezor/blockbook> | `f1d243b82843` | address indexing, ordered block connection, fork rollback |
| `nbxplorer/` | <https://github.com/btcpayserver/NBXplorer> | `f3da40cfc6e3` | watched-source events, input/output attribution, queries and replay |
| `btcpayserver/` | <https://github.com/btcpayserver/btcpayserver> | `583438b1fe45` | invoice payment classification, confirmation and orphan handling |
| `shkeeper/` | <https://github.com/vsys-host/shkeeper.io> | `df1d583d77bf` | invoice addresses, callbacks, confirmation polling and payouts |
| `trezor-firmware/` | <https://github.com/trezor/trezor-firmware> | `ded1c141b643` | hardware message, key, Bitcoin and Ethereum signing protocols |
| `solana-sdk/` | <https://github.com/anza-xyz/solana-sdk> | `dca44b61a9c3` | signer and native transaction/message structure |
| `solana-keychain/` | <https://github.com/solana-foundation/solana-keychain> | `b8f8c49e1a0a` | async remote signer backends and partial signing |

To recreate a missing checkout, use:

```bash
git clone --depth 1 --filter=blob:none <repository-url> reference/<directory>
```

Revisions record what informed the current research; update this file whenever
a checkout is deliberately refreshed and conclusions are revalidated.
