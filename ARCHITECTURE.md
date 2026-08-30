# Architecture

## One process and one composition root

`apps/api` is the only executable and composition root. Its `main.rs` reads and
validates configuration, constructs concrete Bitcoin, Ethereum, and Solana objects,
combines their indexing capabilities, registers wallet families, imports
configured wallets, starts synchronization, builds the router, and supervises
shutdown.

This object graph is implemented in `apps/api`: all configured identities are
verified before one schema-pinned PostgreSQL pool opens, and native SOL joins
Bitcoin and Ethereum through the same wallet and indexing contracts. Redb
remains an SDK repository implementation and deterministic test backend; it is
not the production application composition.

Construction is intentionally visible. There is no process facade, app-local
service facade, separate wallet process, separate indexer process, or internal
wallet/indexer HTTP protocol.

```text
main
  |- one process-wide PostgreSQL pool -----------------------------\
  |    |- Repository(scope = bitcoin/network) -> Bitcoin Service ---+
  |    |- Repository(scope = ethereum/network) -> Ethereum Service -+-> Composer
  |    `- Repository(scope = solana/network) -> Solana Service -----/
  |
  |- Bitcoin/BTC provider/sender -------\
  |- Ethereum/ETH provider/sender -------+-> Wallets(instances, birthdays, Checkpoint)
  |- Ethereum/allowlisted-token provider /
  `- Solana/SOL provider/sender --------/

sync task: Wallets::filters() -> Composer
HTTP: State { Wallets, readiness }
```

Each configured chain has one long-lived JSON-RPC client shared by its indexing
source and wallet-side capabilities. Retry and ordered endpoint failover are
configured once for reads and deterministic preflight; the architecture does
not invent a universal chain RPC interface. Ethereum raw submission is one
attempt against the first configured endpoint, so failover cannot hide whether
an earlier endpoint accepted the envelope. The coordinator owns any exact-byte
replay and reconciliation.

`apps/api` chooses an application-owned asset key for the existing generic
wallet-family parameter. Native ETH and an allowlisted ERC-20 such as USDC are
separate registrations backed by separate `WalletProvider` configurations,
while sharing the same Ethereum RPC and indexing objects. One generated wallet
therefore has one fixed payment asset. This application choice does not add
asset-family state to `sdk/wallets` or `sdk/chains/base`.

An asset is a fact within canonical history, not a persistence scope. Native
and token families on one chain share that chain/network repository handle;
they never receive separate schemas, pools, or repositories.

Token admission is a composition concern. The application validates the
allowlisted contract on one canonical block before erasing the concrete account
client behind wallet capabilities. Until endpoint identities are validated
separately, token-enabled composition admits exactly one Ethereum RPC endpoint.

The concrete `Arc<Composer>` is cloned into narrow `Indexer`, `Checkpoint`, and
`History` trait-object views. Bitcoin separately receives an `Arc<dyn Outputs>`
view of its own repository; `Composer` does not force UTXO semantics onto
account chains. These are views of already-composed state, not parallel
registries.

## Dependency direction

```text
apps/api
  -> sdk/chains/{bitcoin,ethereum,solana}
  -> sdk/wallets
  -> sdk/indexing
  -> sdk/indexing/postgres

sdk/chains/{bitcoin,ethereum,solana}
  -> sdk/chains/base
  -> sdk/wallets
  -> sdk/indexing
  -> packages/{crypto,json-rpc}

sdk/wallets -> sdk/chains/base, sdk/indexing, packages/crypto
sdk/indexing/postgres -> sdk/indexing
sdk/indexing/redb -> sdk/indexing, packages/storage/redb
sdk/indexing -> sdk/chains/base
sdk/chains/base -> packages/crypto
```

- A package may depend on external crates or another package, never SDK/apps.
- `sdk/chains/base` imports no concrete chain, RPC, indexing, or wallets.
- `sdk/indexing` imports no chain-native block, backend record, wallet, or HTTP
  type.
- Concrete chain crates implement generic wallet/indexing contracts while
  retaining their native protocol semantics.
- `sdk/indexing/postgres` and `sdk/indexing/redb` implement persistence
  collections only and own no synchronizer runtime.
- No crate depends on `apps/api`.

## Ownership

### Generic packages

`packages/*` must remain useful outside a blockchain project:

- `crypto` owns secret memory and generic cryptographic operations;
- `http` owns small server/client mechanics;
- `json-rpc` wraps `jsonrpsee` with bounded transport, retry, and endpoint
  failover;
- `storage` owns backend-neutral atomic key/value mechanics;
- `storage/redb` owns the generic redb engine; and
- `design-lint` enforces repository architecture and API rules.

Packages contain no wallet, chain, indexing, asset, or transaction policy.
Before Solana composition, `json-rpc` replaces endpoint/header-bearing derived
`Debug` with manual redaction. Solana uses one singular endpoint and one-shot
execution; its indexing loop and coordinator, not generic transport, own every
explicit retry.

### Chain base

`sdk/chains/base` contains the explicitly approved values and capabilities
that genuinely apply across substantially different chains: addresses,
network/chain/asset metadata, exact decimals, block identity, derivation and
key/signature values, minimal signing, transaction snapshots, and broadcast
results.

It is not a home for UTXOs, Ethereum envelopes, chain RPC DTOs, wallet
construction, indexing policy, or a universal transaction representation.

### Concrete chains

A concrete chain owns everything that disappears when that chain is deleted:

- canonical address parsing and network validation;
- native RPC methods and wire DTOs;
- native block and transaction parsing;
- UTXO/script/fee or account/nonce/gas rules;
- transaction building, signing, encoding, and broadcasting;
- wallet provider, balance implementation, and batch sender; and
- block source and interpreter.

Bitcoin, Ethereum, and Solana share an enforced directory skeleton but may have
different protocol-specific files. Equivalent boundaries use equivalent
directories; native semantics are not flattened to make file names identical.

The `chain-solana` package privately owns Solana addresses, Ed25519
seed/keypair handling, lamports, RPC DTOs, legacy messages/transactions,
account policy, source/interpreter translation, provider/sender behavior, the
source-keyed coordinator, and its one-method submission-task registrar. Its
design-lint layer depends only on packages, base, indexing, and wallets. No
Solana message, account, slot, envelope, or coordinator type becomes a generic
protocol abstraction.

Its protocol stack is the exact modular Anza/SPL dependency family selected in
ADR-0027, not `solana-client`, a monolithic SDK, handwritten wire encoding, or
copied System discriminants. The workspace baseline is Rust 1.91; the reverted
Rust 1.85 experiment is historical evidence, not an active requirement. An
exact scratch-only Rust 1.91 proof combined the current workspace graph with
the selected modular Solana family while retaining `alloy-consensus`,
`alloy-eips`, and `alloy-rpc-types-eth` 1.8.3; `alloy-primitives` and
`alloy-sol-types` 1.6.1; and redb 4.2.0. Repository manifest work must preserve
that graph and repeat the locked Rust 1.91 checks and focused regressions;
divergence stops the affected step.

Deleting one chain must leave the other chain and every generic crate coherent.

### Indexing

`sdk/indexing` owns:

- exact chain/network scopes and complete block-reference checkpoints;
- address filters with native-position birthdays;
- canonical transaction, movement, and live-output facts;
- block-source/interpreter contracts;
- `Blocks`, `Transactions`, and `Outputs` persistence collections;
- confirmation derivation and checkpoint-bound pagination;
- one-scope synchronization; and
- the multi-scope `Composer`.

One-chain `Service` and `Composer` implement the same `Indexer` trait. The sync
caller supplies the authoritative address selection through the chain-neutral
filter source; synchronization owns no durable selection state. Wallet SDK and
embedding-application code own identities, secrets, family selection, and
birthdays. The synchronizer never queries custody state directly; the separate
reusable SDK `Registry` capability remains implemented by PostgreSQL for wallet
adoption/restoration.

`BlockPosition` is the native monotonic RPC coordinate: Bitcoin height,
Ethereum block number, or Solana slot. It drives traversal, canonical lookup,
restart, readiness, and birthdays. `BlockHeight` is the produced-block count
and drives confirmation arithmetic, history and output ordering, rollback
journal keys, and retention. A persisted `BlockRef` carries both coordinates,
its hash, and one atomic optional parent value pairing parent position with
parent hash. Only genesis has no parent.

`Blocks::add` atomically commits canonical history, live output changes, a
storage-derived bounded journal entry, and checkpoint movement. `Blocks::remove`
uses only that private journal to remove an orphan tip and restore live outputs.
`Transactions` and `Outputs` are read projections over this lifecycle. Each
PostgreSQL history or output page uses one read-only repeatable-read transaction
for its checkpoint and projection queries so it cannot mix canonical views;
history movements use that same snapshot.

`apps/api` uses exactly one PostgreSQL database, one shared schema,
and one process-wide connection pool. It constructs one
`indexing_postgres::Repository` per exact `(chain, network)` scope by cloning
that pool. The repository handle enforces scope isolation; the schema is not
duplicated per chain or asset. The canonical central schema creation and
migration history is a deployment concern stored physically under
`sdk/indexing/postgres/migrations/`.
`sdk/indexing/postgres` owns indexing row encoding, set-based statements,
transactions, and compare-and-swap rules. Its add/remove transaction takes a
scope-derived advisory lock before reading the checkpoint, including when the
scope has no checkpoint row, and retains the row lock as a second guard.
Its benchmark uses a unique scope and scope-only dependency-ordered cleanup;
it never truncates the shared schema or deletes SDK registry rows.
Owned PostgreSQL contracts compose multiple scope-bound repositories from that
one pool, preserve native/token facts, reject cross-scope reads and writes, and
compare all unrelated scope rows across a neighboring commit.

Physical migration colocation does not erase capability boundaries. A script
touching the SDK registry table `payment_wallets` requires explicit SDK-level
custody approval and preservation evidence. Synchronizer repository operations
still must not query, mutate, or issue DDL for that table; only the existing
registry capability may read or write it.

`sdk/indexing/redb` remains an embedded persistence implementation and test
backend, but it is not the application composition. Backend records never
appear in a chain interpreter or generic consumer.

The synchronizer's durable set is deliberately limited to checkpoint,
address-primary canonical history, live outputs, and a bounded rollback
journal. Confirmation, readiness, status, watches, revisions, raw blocks, and
event feeds are not synchronization persistence. Process-local submission
leases, exact outgoing envelopes, request identities, and reconciliation state
are also not PostgreSQL or indexing-owned records.

The physically colocated `payment_wallets` table belongs to the reusable SDK
registry/restoration and custody-integration path. It is not checkpoint,
history, output, or journal state, and scope-local indexing operations never
touch it. Existing registry rows remain byte-for-byte preserved. This does not
certify the current opaque secret bytes as production custody; custody policy
and a future encrypted implementation remain separate decisions. The existing
SDK registry path is preserved rather than moved exclusively into `apps/api`.

### Wallets

`sdk/wallets::Wallets<I, F>` owns the application-facing collection:

- a family map from `F` to scope, provider, and sender;
- abstract wallet instances and public metadata keyed by `I`;
- authoritative canonical address native-position birthdays; and
- the shared `Checkpoint` capability used to choose safe runtime birthdays.

`Provider` constructs a concrete wallet by generating or importing secret
material without returning that secret. A separate provider map is redundant;
family registration owns the provider exactly once.

`Wallets` exposes startup-only import plus chain-neutral runtime generation,
get, balance, history, one send, and batch send operations. It delegates native
behavior to registered wallets and senders. Business and endpoint code do not
match on a concrete chain.

For `payment-api`, `F` is the closed `WalletAsset` selector (`btc`, `eth`,
`usdc`, or `sol`), not merely a chain identifier. The collection remains
generic and other embedding applications may choose a different key type.

The wallet/key registry is in memory. Durable custody is the embedding
application's responsibility and is not represented as an indexing concern.

### Public HTTP

`apps/api` owns public routing, Utoipa schemas, transport validation,
authentication, limits, HTTP errors, and encoding. HTTP state contains the
abstract wallet collection and readiness state, not repositories, sources,
interpreters, or concrete chains.

Handlers are grouped by resource. Every endpoint-specific request and response
struct is declared immediately above its one handler. A handler is limited to
extraction, one `Wallets` operation, error/status mapping, and encoding. Shared
wire types exist only for exact reuse; domain types remain with their domain.

## Indexing flow

```text
Wallets::filters()
    -> Composer::sync
    -> partition filters by IndexScope
    -> chain Service
    -> verify local checkpoint against canonical hash
    -> retained reorg removal when needed
    -> source produced blocks after the checkpoint position
    -> interpreter(native block, active addresses)
    -> BlockAddition::new
    -> Blocks::add
```

All configured historical wallets are registered before the first sync. A
fresh scope locates the first produced block at or after its earliest birthday
position and uses that block's real parent as the anchor; an empty filter set
anchors at the actual produced source tip. A generated wallet begins at the
checked successor of the current checkpoint position. If that native position
is skipped, the wallet activates at the first later produced block.

The persisted checkpoint is valid for the authoritative historical address set
that produced it. A changed set below the checkpoint requires recreating and
rescanning the scope, because synchronization resumes from the checkpoint and
never revisits blocks behind it.

On restart, the application reloads the authoritative wallet/birthday set
before synchronization. A changed birthday beneath the checkpoint requires an
explicit rescan of only that exact indexing scope. Such a rescan must never
drop the central database, touch another chain/network scope, or delete
application-owned `payment_wallets` rows.

A retained reorg removes orphan blocks until the common ancestor, then indexes
the replacement branch normally. When the ancestor is outside retention,
`ReorgTooDeep` requires a scope rescan.

## Transaction flow

```text
abstract wallet request
    -> concrete chain-native builder
    -> unsigned native transaction
    -> chain computes signing request
    -> injected Signer
    -> chain verifies and inserts signature
    -> exact signed bytes
    -> native broadcaster
    -> submitted transaction ID
    -> indexing observes canonical inclusion/confirmation/reorg
```

Submission is not confirmation. No broadcaster polls receipts to establish a
second confirmation system.

Bitcoin preserves outpoints, every input/output, scripts, checked satoshi
fees, per-input signers, and deterministic change. Ethereum preserves chain ID,
nonces, EIP-1559 fees, typed envelopes, recovered signer, receipts, logs, and
the configured native-or-token asset. An ERC-20 wallet exposes only that token
for balance, send, and history movements while retaining native ETH as network
fee metadata. The shared wallet surface does not replace those native models.

For batches, validate every request and the one-family constraint before the
first external effect. Bitcoin may produce one multi-source native transaction.
One Ethereum-owned coordinator is shared by native and token providers. It
reserves consecutive nonces by sender address, completes whole-batch
simulation, cumulative balance checks, and signing, then broadcasts the exact
envelopes in request order. A retryable ambiguous submission retains its exact
envelope and blocks that sender until exact-hash reconciliation; a later
failure reports only the accepted prefix.

The Solana-owned coordinator acquires source addresses in canonical order and
builds one legacy System-transfer-plus-Memo transaction per public occurrence.
It uses exactly the executable Memo-v3 program account, obtains exact fees,
checks cumulative lamports, signs, and simulates the complete batch before
ordered one-shot, endpoint-stable broadcast. An unknown result may replay only
the same bytes within the recent-blockhash lifetime and guards the source until
status or complete finalized indexed history resolves observation or absence.
Startup and the owned validator fixture require that exact Memo program to be
executable. The ADR-0027 concrete probe and supervised task wiring are
implemented; the checksum-pinned real-validator execution remains outstanding
system evidence.

Ethereum and Solana coordinator state is intentionally in-process because
indexing owns no pending-transaction records and the product has no durable
outgoing-operation store. Application composition therefore requires one
active transaction writer per managed Ethereum EOA or Solana source. Solana
callers must not automatically retry an unknown logical payment; restart,
failover, or active-active writers can double-pay, and process-crash recovery is
not claimed.

## Runtime lifecycle

Startup order is part of correctness:

1. validate configuration and server security;
2. construct each configured chain client, including Solana's singular,
   no-retry client with redacted configuration;
3. verify every chain identity before database mutation;
4. for Solana, verify the expected Base58 genesis hash and the finalized,
   contextual executable state of the exact Memo-v3 account;
5. open one process-wide PostgreSQL pool, pin and validate the already-applied
   shared schema without mutating it;
6. clone one scope-bound repository handle per configured chain/network, load
   checkpoints, and initialize scope filter/commit coordination;
7. construct chain services and `Composer`, then inject the Solana service's
   narrow `Checkpoint`, `History`, and checkpoint notification into its
   submission coordinator;
8. construct `Wallets`, register only the accepted families, and import every
   configured seed at `start_position` before the first sync snapshot;
9. start synchronization and the application-owned readiness/submission
   supervisors;
10. wait for every configured scope to report `Ready` with a persisted
   checkpoint;
11. bind the public listener; and
12. supervise HTTP, synchronization, Ctrl-C, cancellation, submission
    reconciliation, and task joins.

`SyncStatus` reports only progress (`CatchingUp` or `Ready`). Failures are typed
errors, not cached status variants. A fatal synchronizer exit fails startup. At
runtime it publishes not-ready and closes new HTTP admission rather than serving
stale data. With no guarded envelope the process joins and returns the fatal
error; with a submitted or ambiguous Solana envelope it enters the accepted
shutdown barrier instead of erasing the only reconciliation state.

The application owns one `mpsc` admission queue and `JoinSet` for Solana
submission. Its registrar acknowledges only after task insertion, so closure or
lost acknowledgement before insertion fails before dispatch. Account
acquisition completes before registration. No handler, wallet, or chain object
may detach a send or readiness task outside application supervision.

Graceful shutdown publishes not-ready, stops HTTP admission, serializes registrar
closure against task insertion, drains handlers, and waits for the guarded set
to become empty while indexing and historical-status reconciliation remain
available. Only then does it cancel synchronization and await storage work. An
unknown envelope has no automatic shutdown deadline. After a fatal indexer exit,
only positive historical status may clear it in-process; force-kill explicitly
accepts the documented duplicate-payment risk.

The integration environment pins `solana-test-validator v3.1.14` at commit
`3134055b562e95902233be308453fffa1c4a8902`, verifies committed SHA-256 values,
and owns ledger, ports, keys, and cleanup. Default tests use RPC doubles. The
explicit `solana_stack` application target, checksum gate, resource harness,
and end-to-end scenario are implemented. It remains manual rather than CI
automation until a checked-in workflow owns the pinned fixture; no run is
claimed unless the exact artifact is locally available and verified.

Shared-schema evolution is preservation-first. Indexing migrations are
additive or backfilled under explicit validation before final constraints are
enforced. They do not introduce runtime compatibility readers, versioned DTOs,
or inferred fallbacks. A destructive scope-local replacement requires explicit
operational approval and may remove only indexing-owned rows for the named
scope.

## Product boundary

The architecture supports wallet generation/import, canonical
address and exact selected-asset balance, complete checkpoint-bound paginated
history for that selected asset, one or ordered batch submission, and
continuous filtered indexing. Indexing may retain unrelated canonical facts for
a watched Ethereum address; the concrete wallet projects only its configured
asset.

It does not contain deposit accounting, ledgers, payment state machines,
collection/sweep jobs, reservations, hardware-wallet workflows, remote
custody, public index-management commands, raw-block archives, event feeds, or
pre-release compatibility layers.
