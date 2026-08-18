# Design lint

`design-lint` enforces the small set of architecture constraints the project
has explicitly chosen. It is independent of chain code and configured by
`lint.toml`.

```bash
cargo run --locked -p design-lint -- --policy lint.toml check .
cargo run --locked -p design-lint -- --policy lint.toml --markdown .
cargo run --locked -p design-lint -- --policy lint.toml --cases lint .
```

Diagnostic mode fails on findings. Markdown prints a review document. Cases
refreshes `lint/errors` while retaining `.gitkeep` files.

## Rules

| Rule | What it protects |
|---|---|
| `dependency-direction` | Local Cargo dependencies follow configured layers and contain no cycles. |
| `owned-vocabulary` | Chain names stay in their owning chain or composition paths. |
| `trait-method-count` | Traits contain at most three cohesive functions. |
| `empty-struct` | Structs carry meaningful state. |
| `struct-word-count` | Struct names contain at most two semantic words; trailing version suffixes are ignored. |
| `self-constructor-static` | Constructor-shaped functions returning `Self` are associated functions, not receiver methods. |
| `file-length` | Production Rust files stay below the configured physical line limit. |
| `forbidden-path` | Deleted architecture does not return. |
| `empty-directory` | Repository-owned directories contain real source. |
| `chain-layout` | Concrete chain crates share the configured indexer, RPC, transaction, operations, source, and wallet directory topology; protocol-owned files inside those directories may differ. |
| `single-file-directory` | Every Rust module directory under `src` contains zero or at least two direct Rust files, so each namespace earns its directory. |

An exceptional Rust declaration may suppress a source rule on its declaration
line or either of the two preceding lines. A specific rule and reason are
mandatory:

```rust
// design-lint: allow self-constructor-static -- conventional consuming conversion
fn from_parts(self, parts: Parts) -> Self { /* ... */ }
```

Do not add speculative style checks. A new rule needs an explicit architecture
invariant, a concrete failure it prevents, and focused positive/negative tests.
