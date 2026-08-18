# Indexing design review

## Decision

Keep indexing as one generic SDK domain with chain-owned sources/interpreters,
five storage-independent persistence traits, and a RocksDB implementation.
`apps/api` embeds and supervises every configured `Synchronizer`.

Rejected structures include a separate service or HTTP client, a storage-owned
runtime/handle, chain code producing physical keys, a universal RPC interface,
a fake one-movement UTXO model, event-feed infrastructure, raw-block archives,
backfill/rebuild commands, watch deactivation, and pre-release migrations.

## Vocabulary

| Name | Meaning |
|---|---|
| `BlockSource` | reads canonical chain-native blocks from RPC |
| `BlockInterpreter` | converts one block and address-watch snapshot into semantic facts/effects |
| `InterpretedBlock` | result ready for one atomic repository commit |
| `Synchronizer` | orders source reads, canonical checks, reorg rollback, interpretation, and commits |
| `Index<R>` | consumer facade for checkpoint, watch registration, and history |
| `Repository` | physical RocksDB implementation of semantic persistence traits |

Generic indexing owns scope, block identity, address watches, normalized
observations and movements, revision history, synchronization, and reorg
ordering. Concrete chains own parsed blocks, RPC DTOs, scripts/logs, and the
effect/undo data necessary to update their projections.

## Trait boundaries

Consumer behavior is `Checkpoint` (one method), `Watcher` (one method), and
`History` (two methods). Persistence is split by invariant into
`CanonicalStore`, `WatchStore`, `BlockStore`, `HistoryStore`, and `StatusStore`;
each has exactly two methods. This keeps traits object/composition friendly
without separating naturally paired operations.

The application shares a repository clone between `Synchronizer` and `Index<R>`.
HTTP handlers receive wallet/indexing capabilities, never RocksDB, source, or
interpreter objects. A PostgreSQL implementation should be possible by
implementing the same five traits; if a chain interpreter mentions a key
prefix, column family, table, or SQL row, ownership is wrong.

## Review checks

- Keep every public type tied to a current runtime invariant.
- Preserve canonical height+hash, atomic commits, undo, and observation
  revisions across restarts and reorgs.
- Keep all physical record encoding private to the storage adapter.
- Do not add dormant transport, event, rebuild, migration, or archive surfaces.
- Prove chain deletion, restart, and reorg behavior with compile/system tests.
