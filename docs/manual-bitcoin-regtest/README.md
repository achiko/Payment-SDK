# Bitcoin regtest boundary

The repository has a narrow, opt-in live-node acceptance harness. It starts a
disposable local Bitcoin Core 31.x process with regtest, networking disabled,
and an isolated temporary datadir. It matures disposable mining funds, starts
the concrete Bitcoin indexer with temporary RocksDB storage, creates and funds
a concrete SDK wallet, waits for its indexed balance, builds and signs through
the wallet abstraction, broadcasts through the production RPC client, mines
inclusion, and asserts confirmed wallet history and the resulting balance. It
also verifies node identity and canonical genesis. It cannot be configured
with an existing RPC URL.

The checked-in deterministic indexer E2E test exercises Bitcoin block reading,
the real indexer HTTP runtime, temporary RocksDB persistence, watches, history,
events, output queries, shutdown, and restart without requiring Bitcoin Core:

```bash
mac cargo test --locked -p indexer-worker --test runtime_e2e bitcoin -- --nocapture
```

The live test has two independent opt-in gates: its Cargo target requires the
`live-bitcoin-core` feature and the test itself is ignored. The wrapper supplies
both gates and requires absolute paths; it never downloads binaries:

```bash
BITCOIND=/absolute/path/to/bitcoind \
BITCOIN_CLI=/absolute/path/to/bitcoin-cli \
./tests/system/run-bitcoin-core-acceptance.sh
```

Default workspace tests neither compile this target nor start a node. A
successful compile proves only that the harness matches current Rust APIs. A
successful opt-in run additionally proves the listed wallet/indexing/RPC flow
against the reported local Core 31.x binary. It does **not** exercise the HTTP
payment/deposit application, production custody, public networks, HA, or funded transactions.
No live or funded transaction is authorized by this document.

Evidence checked on 2026-08-18: the feature-gated target compiled successfully,
and a default `system-tests` build omitted it. No `bitcoind` or `bitcoin-cli`
binary was available in that validation environment, so this checkout does not
claim a successful live-node run.
