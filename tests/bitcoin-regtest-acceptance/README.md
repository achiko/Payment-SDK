# Automated Bitcoin Core 31.1 regtest acceptance

This package is the opt-in, non-LLM black-box system-acceptance harness for the
composed Bitcoin Core, IX, development custody, WS, and PS stack. Ordinary
`cargo test` validates the harness without starting a node. The live runner is
never invoked by the workspace test suite.

The default profile runs four isolated scenarios in both strict and
global-trusted authentication modes:

- P0 P2WPKH/P2TR sign-before-broadcast and exact-byte submission;
- P1 IX/WS/PS restart and durable replay;
- P0 controlled rollback, UTXO restoration, same-byte retry, and re-inclusion;
- P0 competing exact-outpoint collection ownership.

The complete local validation run on 2026-08-11 passed 8/8 against Bitcoin
Core 31.1.0 (`btc31-019ff25b-7765-7250-9014-0a98d16fc2c0`). Generated reports
remain untracked build artifacts; rerun the suite to produce checkout-specific
evidence.

## Prerequisites and command

Provide explicit, executable Bitcoin Core 31.1 paths. The runner verifies both
binary version strings and the live node's numeric version. It never downloads
or replaces Bitcoin Core.

```bash
BITCOIND="$PWD/bitcoind" \
BITCOIN_CLI="$PWD/bitcoin-cli" \
./scripts/run-bitcoin-regtest-acceptance.sh
```

Run one isolated case while developing the harness:

```bash
BITCOIND="$PWD/bitcoind" \
BITCOIN_CLI="$PWD/bitcoin-cli" \
./scripts/run-bitcoin-regtest-acceptance.sh \
  --mode strict \
  --scenario signing
```

The runner also accepts `--artifacts-dir <path>` and `--keep-workdir`.
`--mode` accepts `strict`, `global-trusted`, or `all`; `--scenario` accepts
`signing`, `restart-replay`, `reorg`, `reservation`, or `all`.

Each run writes `summary.json`, `junit.xml`, and sanitized process logs beneath
`target/bitcoin-regtest-acceptance/<run-id>/`. The private Core datadir,
databases, RPC cookie, service credentials, key locators, and signed bytes are
deleted by default.

`--keep-workdir` is a local diagnostic escape hatch. Its reported directory
contains private disposable fixture material and must not be copied into the
repository, CI artifacts, tickets, or chat.

## Evidence boundary

A passing live run is operational evidence only for Bitcoin Core 31.1 regtest
and the exact service binaries built with it. It does not establish production
custody, public-network behavior, HA, mempool replacement/drop tracking, or
mainnet readiness. Repository unit and real-store integration tests remain the
separate proof of storage-level atomic batches and deterministic edge cases.
