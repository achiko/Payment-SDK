# Design lint

Repository-independent Rust and repository-structure design checks. The crate has no dependency on Husklet code, policy, paths, or C tooling.

Run it from the workspace root:

```bash
cargo run --locked -p design-lint -- --policy lint.toml .
cargo run --locked -p design-lint -- --policy lint.toml --markdown .
cargo run --locked -p design-lint -- --policy lint.toml --cases lint .
```

Diagnostic mode exits unsuccessfully when an error-severity rule has findings. Markdown mode prints reviewable findings and a count for every rule. Cases mode refreshes persistent review files under `lint/errors` and `lint/check`.

## Rules

| Rule | Severity | Validation |
|---|---|---|
| `dependency-direction` | error | Local Cargo dependency cycles, directory or exact-package layer direction, dependency budgets, and proven module-layer edges. |
| `documentation-contract` | error | Configured Markdown inventory and structural example-document contracts. |
| `runtime-tool-ownership` | error | Configured tool/audit packages do not live in protected runtime domains. |
| `owned-vocabulary` | error | Configured implementation-specific words appear only in their explicitly owning source paths. |
| `unsafe-boundary` | error | Unsafe code is confined to configured boundaries and carries an attached safety rationale. |
| `unclassified-free-function` | error | Free functions over owned domain types are moved to an owner or explicitly classified for review. |
| `detached-constructor` | error | Functions constructing a concrete owned type live on the constructed type or a genuine factory owner. |
| `duplicate-entity-base` | error | Structs do not duplicate a meaningful common field base instead of composing it. |
| `boolean-state-cluster` | warning | Related booleans do not encode an implicit finite state machine or invalid combinations. |
| `broad-trait-responsibilities` | warning | Traits do not combine several independent capability clusters. |
| `trait-method-count` | error | Every production trait declares at most three functions, including provided methods. |
| `environment-variable-access` | error | Ambient process configuration is captured only at configured composition boundaries. |
| `repository-escape` | error | Cargo paths and source includes do not escape repository or crate ownership. |
| `manual-cli-dispatch` | error | Long-option parsing is typed rather than implemented with mutable index/cursor dispatch. |
| `platform-command-boundary` | error | Host commands and shells are created only at configured platform/composition boundaries. |
| `ignored-fallible-result` | error | Fallible calls and awaited operations are consumed, returned, or deliberately discarded. |
| `async-blocking-operation` | error | Async scopes do not call known blocking filesystem, process, thread, or synchronization operations directly. |
| `struct-noun-naming` | error | Struct names are nouns rather than actions or past-tense conditions. |
| `package-name-prefix` | error | Internal type declarations do not repeat their Cargo package namespace; compatibility aliases may do so at re-export boundaries. |
| `receiver-name-repetition` | error | Method names do not redundantly repeat their receiver type. |
| `god-object-growth` | warning | One receiver does not accumulate several unrelated capability/workflow clusters. |
| `redundant-accessor` | warning | Field-only accessors and wrapper APIs add an invariant or behavior rather than pure forwarding. |
| `wire-domain-model-duplication` | warning | Wire and domain structs with the same shape compose or translate deliberately instead of drifting copies. |
| `maximum-nesting` | error | Operational control flow stays within two meaningful nesting levels, with guard/value-flow exemptions. |
| `file-length` | error | Production Rust files stay within the rule's effective 500-line boundary after test/declaration exclusions. |
| `file-name-density` | error | Source filenames contain at most two semantic words and use a directory for the broader noun. |
| `redundant-parent-name` | error | A source filename does not repeat the semantic noun already supplied by its parent directory. |
| `singular-test-file` | error | A lone companion test is kept beside its owner instead of creating a detached test file. |
| `flat-prefix-density` | error | Repeated filename prefixes become a noun directory rather than a flat pseudo-namespace. |
| `flat-role-density` | error | Repeated role suffixes such as adapter/client/manager do not replace domain grouping. |
| `path-module-flattening` | error | Path-selected modules do not flatten several injected architectural domains into one file. |
| `test-only-source-directory` | error | Production source trees do not contain directories used only for tests. |
| `sibling-test-dependency` | error | Test modules do not depend on sibling tests instead of explicit test support. |
| `test-suite-kebab-path` | error | Integration-test suite paths use kebab-case. |
| `integration-test-candidate` | warning | Tests using only public APIs are reviewed for movement to an integration suite. |
| `folder-noun` | error | Nontrivial source folders are justified noun namespaces rather than arbitrary structure. |
| `redundant-module-prefix` | error | Items inside a module do not repeat the module's semantic prefix. |
| `string-backed-finite-state` | warning | Repeated string literals used as state are replaced with a typed enum. |
| `catch-all-module-name` | error | Rust modules avoid generic catch-all names such as common, shared, helpers, or utils. |
| `catch-all-source-path` | error | Source files/directories avoid reserved catch-all path names without scoped evidence. |
| `empty-directory` | error | Repository-owned directories are not empty or placeholder-only. |
| `single-file-directory` | error | A nonconventional directory has enough cohesive content to justify its namespace. |
| `ceremonial-structure` | warning | Marker traits, transparent namespaces, and forwarding wrappers have a real contract or behavior. |

Repository-specific exceptions belong in `lint.toml`; they must not be hard-coded into rule implementations.

One exceptional declaration can suppress a finding with an attached reason on
the declaration line or either of the two preceding lines:

```rust
// design-lint: allow detached-constructor -- required by an external callback ABI
fn construct_record() -> Record { Record }
```

The rule ID and non-empty `-- reason` are required. Unreasoned or blanket
suppression comments are ignored.
