# PR #34 Review — Rust Foundation and Graphiti Compatibility

Reviewed with Astra against `main`, including the complete committed diff, the 28
published Copilot comments, suppressed review findings, the live deployment, and
the uncommitted worktree changes.

## Verdict

**Do not merge PR #34 yet.**

Copilot's review is substantially correct. The current root-host deployment masks
several portability defects, and Astra found additional correctness and release
blockers that were not raised in the published inline comments.

## Release blockers

### 1. A fresh checkout does not reproduce the working BGE-M3 deployment

The llama.cpp/OpenAI-compatible embedding support used by the live installation is
still uncommitted in:

- `bin/engram_llm.py`
- `bin/memory_recall.py`
- `tests/test_recall_fusion.py`

Without those changes, the committed Python Graphiti path recognizes only Ollama
and FastEmbed. A fresh install using the documented BGE-M3 configuration can fall
back to a 768-dimensional FastEmbed model while the graph expects 1024 dimensions.
Local success therefore does not prove that PR #34 works after a clean checkout.

### 2. Native triple replacement uses invalid Cypher membership logic

`crates/engram-graph/src/lib.rs` uses:

```cypher
NOT [triple IN $triples | triple.key] CONTAINS obsolete.key
```

`CONTAINS` performs string matching and is not list membership. Astra verified the
exact expression against the installed Neo4j; it evaluates to `null`, so removed or
changed triples are not selected for retirement. It should use membership syntax,
such as:

```cypher
NOT obsolete.key IN [triple IN $triples | triple.key]
```

Until corrected, obsolete native triples can remain active indefinitely.

### 3. Normal-user installation paths and slugs are incorrect

Several Rust entry points hard-code `/root/.claude`, `/root/.claude/graph`, or the
`-root` memory slug:

- `crates/engram-app/src/bin/engram-mcp.rs`
- `crates/engram-app/src/bin/engram-recall-hook.rs`
- `crates/engram-app/src/bin/engram-recall.rs`
- `crates/engram-app/src/bin/engram-graph-recall-eval.rs`
- `crates/engram-app/src/main.rs`
- `crates/engram-hybrid/src/lib.rs`

The current machine works because the service runs as root. Other users can load
the wrong configuration, graph environment, and memory store. Config path, Engram
home, graph directory, and canonical slug need a shared resolver with this order:

1. Explicit command-line option.
2. Environment override (`ENGRAM_CONFIG`, `ENGRAM_GRAPH`,
   `CLAUDE_MEMORY_SLUG`).
3. Existing `engram.env` slug pin.
4. Home-derived default.

The installer must also pass the resolved values when registering MCP servers and
hooks.

### 4. The Rust recall hook can block every prompt indefinitely

Graphiti is invoked with blocking `Command::output()` and no deadline in
`crates/engram-hybrid/src/lib.rs`. Because `engram-recall-hook` calls that path from
`UserPromptSubmit`, a stalled Neo4j or Graphiti request can indefinitely delay a
prompt.

The hook also:

- ignores `recall.inject.enabled`;
- hard-codes the `-root` store;
- never expires old session-state files;
- does not apply a bounded recall timeout;
- fails open only after the blocking operation eventually returns.

The compatibility worker protocol should carry a request deadline, terminate a
timed-out child, and return an explicit error. The hook should use a shorter budget
and remain fail-open.

### 5. Upgrades may run both recall hooks

`install.sh` adds the Rust hook when its binary exists but does not remove the
previous Python `memory-recall-inject.py` hook. Existing installations can execute
both hooks with independent deduplication state, causing duplicate work and
possibly duplicate memory injection.

The installer must migrate the old hook entry atomically instead of only appending
the new command.

### 6. Rust indexing is selected for providers it does not implement

`engram-index` constructs only an OpenAI-compatible embedding client from
`embed.url`. The daemon and save hook prefer that binary whenever it exists, even
for supported legacy configurations using:

- automatic provider selection;
- Ollama embeddings;
- FastEmbed with an empty URL.

Affected integration points include:

- `crates/engram-app/src/bin/engram-index.rs`
- `daemon/engram-daemon.py`
- `bin/memory_lib.sh`

The same issue affects native graph synchronization, which unconditionally uses
`llama_cpp.url` and `embed.url`. Either implement every advertised provider in Rust
or select Rust only when the configured providers are supported, retaining the
working Python path otherwise.

### 7. Native sync can mark incomplete data as current or permanently skip it

`engram-native-graph-sync` updates an `EngramMemory` node and its SHA before fact
extraction, embeddings, and triples have all committed. On a previously versioned
record, a later failure can leave the new SHA together with the old current-version
marker. The next run may then skip the incomplete memory.

Embedding failures are also silently discarded before the memory is marked
current. A transient embedding outage can produce a permanently incomplete
semantic index.

Use a generation/fingerprint and commit marker that is written only after every
required stage succeeds. Failed memories must remain retryable.

### 8. Native graph identity is not scoped by memory store

Native graph nodes are keyed only by filename, even though filenames are unique
only inside `projects/<slug>/memory`. Two stores with the same filename can merge or
overwrite one another, and native recall does not filter by slug.

The store slug must be included in:

- `EngramMemory` identity;
- fact and triple identity;
- current-state checks;
- semantic and keyword recall predicates;
- stale-data reconciliation;
- legacy embedding import.

### 9. Historical and superseded claims can remain current

High-confidence triples with temporal state `formerly` are stored with active
status and no `valid_until`, so native recall can present historical state as
current.

The `supersedes` update also searches for an older triple with the same
`subject + supersedes` relation instead of identifying the prior claim represented
by the superseded object. Old `uses`, `runs_on`, or equivalent claims can remain
active.

Temporal status and supersession need explicit, tested semantics rather than
relation-name inference inside the storage query.

### 10. The current evidence does not prove complete 1:1 Graphiti behavior

The compatibility path preserves ordered filenames for the manually tested query,
but the Rust response does not preserve the full Graphiti result structure:

- facts are flattened into one global list instead of remaining associated with
  each source memory;
- neighbour records are not included;
- engine/index version metadata is absent;
- error and timeout semantics differ;
- score ties and concurrent requests are not covered.

The bundled evaluator compounds this problem:

- it compares `GraphClient::keyword_files`, which is raw Neo4j full-text search,
  rather than `memory_graph_recall.py`;
- it converts both sides to `HashSet`, discarding order;
- it therefore measures overlap, not Graphiti recall parity.

The parity suite must invoke the actual pinned compatibility worker and compare
ordered records, per-record facts, neighbours, empty results, duplicate episodes,
ties, errors, and timeouts against frozen fixtures.

### 11. Documented authentication settings are omitted in Rust

The Rust configuration and clients drop credentials already supported by the
shipped configuration:

- `llama_cpp.api_key` / embedding API key;
- `vector_store.api_key` for authenticated Qdrant;
- Neo4j user and password from the installer-managed `graph/.env`;
- configurable Neo4j URI/database/user consistency.

Consequences include failed authenticated model calls, Qdrant Cloud failures, and
native graph failures under the standard installer credential layout.

Secrets must be carried through internal configuration without being serialized by
the public configuration/status APIs. HTTP clients must attach the appropriate
authorization headers on every request.

### 12. Missing graph configuration defaults upgrades to the wrong backend

`install.sh` preserves existing `engram.yaml` files. Those installations usually
have no `graph` block, but Rust currently defaults a missing backend to `native`.
That silently moves upgraded users away from their populated Graphiti index and
onto an unproven native path.

Missing configuration must default to `graphiti_compat`, or the installer must
perform an explicit, backed-up migration.

### 13. Rust indexing and native extraction weaken the established secret boundary

`engram-index` replaces the shared Python secret detector with a small assignment
regex. It does not cover several credential classes already recognized by
`engram_secrets.py`, including bearer tokens, private keys, mnemonic phrases, and
vendor-prefixed credentials. Native graph extraction sends memory descriptions and
bodies to the configured reasoning endpoint without applying the established
redaction contract.

Normal save guards reduce exposure for newly created memories, but imported files,
direct edits, and existing stores remain supported inputs. Rust paths must reuse an
equivalent tested detector before off-host model or embedding calls.

## Important correctness and UX findings

### Frontmatter type parsing

`crates/engram-store/src/lib.rs` ignores indented frontmatter. Canonical memories
store type as:

```yaml
metadata:
  type: project
```

Rust therefore defaults these memories to `reference`, corrupting Qdrant payload
types and filters. Use a real YAML frontmatter parser or explicitly support the
nested metadata schema.

### Embedding-space freshness

Index freshness hashes only memory content. Changing to another embedding model of
the same dimension does not invalidate existing Qdrant or native graph vectors.
The UI may warn about reindexing, but a normal non-rebuild run still considers
those records current.

Freshness must include an embedding-space fingerprint covering provider, endpoint
identity where relevant, model, prefixes, output dimension, and normalization
settings.

### Embedding prefixes

Configured `document_prefix` and `query_prefix` are not consistently applied to
Rust indexing, vector recall, and native fact embeddings. Models that require
asymmetric prefixes are indexed and queried in incompatible spaces.

### Native fact output

Native keyword and semantic queries return per-hit facts, but the hybrid layer
discards those associations and returns only a separate token-derived triple list.
Ordinary extracted facts may not reach callers.

### Deleted and renamed memories

Native synchronization upserts current Markdown files but does not remove or retire
graph data for files that were deleted or renamed. Stale claims can remain eligible
for recall.

### Config-save concurrency

The Rust API checks a revision, rereads and validates the file, then writes without
holding a lock or rechecking immediately before rename. Another writer can modify
the config between the first check and the final replacement.

The revision check and atomic replacement need serialization through an exclusive
lock or equivalent compare-and-swap mechanism.

### Atlas numeric inputs

React number inputs store `event.target.value`, which is a string. Rust expects
numeric JSON fields for `timeout_seconds` and `dim`, so editing either value makes
validation/save fail deserialization. Convert these values with `Number(...)` or
accept validated numeric strings at the API boundary.

### Atlas save feedback

The save handler calls `setPreview(result)` and immediately calls `load()`, which
resets the preview to `null`. Successful backup and reindex feedback disappears
before users can see it.

### Model health presentation

The Atlas adapter may derive `reachable: true` from the presence of an endpoint and
absence of an error even when the Rust status endpoint did not probe that provider.
Only report reachability when an actual probe occurred.

### Remote Neo4j configuration

The Rust graph transport rejects remote hosts and mentions an explicit HTTPS
endpoint, but exposes no complete configuration path for supplying and validating
that endpoint. Either implement the supported remote transport contract or clearly
limit native graph mode to local Neo4j.

### Read/write backend and configuration precedence

Rust recall honors `ENGRAM_GRAPH_BACKEND`, but daemon writer selection reads only
YAML. The reader and scheduled writer can therefore target different indexes. The
daemon also passes `$ENGRAM_BIN/engram.yaml` to child commands instead of preserving
the resolved `ENGRAM_CONFIG`. Vector and graph jobs can act on a different
installation than the API or MCP server.

One shared resolver must define precedence and be used by every service and child
process.

### Configuration file safety details

Beyond the revision race, config writes use a fixed temporary pathname and
second-resolution backup names. Concurrent requests can collide, and newly created
backups do not explicitly preserve restrictive source permissions. Use an
exclusive lock, unique private temporary/backup files, an under-lock revision
recheck, preserved mode, and atomic replacement.

### Native observability

Several native graph calls use `unwrap_or_default()` and later report the graph leg
as `ok`. Authentication, query, schema, and network errors can appear as successful
empty recall. Preserve structured leg errors and do not mark unavailable native
search as healthy.

### Master enable switch

Rust configuration omits `local_enabled`. The save-triggered Rust index path checks
only `vector_store.enabled`, bypassing the legacy combined enablement rule. The
master switch must keep its documented authority after migration.

### Compatibility-worker architecture drift

The main recall path invokes the Python script directly instead of using the added
`engram-graph-compat` worker. The standalone worker converts child failures into
successful empty arrays. Either use and harden the supervised worker boundary or
remove the unused binary; two different compatibility implementations should not
coexist.

## Findings mitigated only by the current deployment

These are not false positives; the current host merely does not exercise them:

- Root path and `-root` slug bugs are hidden because Engram runs as root here.
- Qdrant API-key omission is hidden by the local unauthenticated Qdrant instance.
- Native Neo4j credential and temporal bugs are hidden while
  `graphiti_compat` remains selected.
- Ollama/FastEmbed Rust-indexer failures are hidden by the current dedicated
  OpenAI-compatible BGE-M3 endpoint.
- Remote Neo4j support is irrelevant to the current loopback database but remains
  an advertised portability gap.

The intentional decision to disable outer keyword/vector fusion while
`graphiti_compat` is selected is not itself a defect: direct Graphiti ordering was
chosen to avoid changing its rank. The documentation must describe that behavior
consistently rather than calling compatibility mode hybrid fusion.

## Dirty worktree assessment

The following user-owned changes were intentionally left out of PR #34:

- `bin/engram_llm.py`
- `bin/memory_recall.py`
- `graph/graph_sync.py`
- `graph/memory_graph_insert.py`
- `tests/test_recall_fusion.py`

Assessment:

- The provider repair in `engram_llm.py` is required before merging, but its new
  catch-all fallback must be corrected first. A failed BGE-M3 request currently
  falls back to an incompatible FastEmbed space instead of failing clearly.
- `memory_recall.py` should ship with that prerequisite repair because the retained
  Python hook/rollback path still depends on it.
- `tests/test_recall_fusion.py` should ship with the provider repair and be extended
  to cover authentication, timeouts, failures, and refusal to cross embedding
  spaces.
- The corrected instruction in `graph_sync.py` is a valid small maintenance repair
  and can be committed separately.
- `memory_graph_insert.py` must **not** be committed as currently written. Its final
  SHA comprehension marks every Markdown file synchronized, including files that
  were skipped, changed but not inserted, excluded by `--only`, or not committed
  because of partial failure. Update only successfully committed files while
  preserving previous state.
- Leaving these changes only on the deployed host makes the release behavior
  materially different from the reviewed branch.

## Test and CI assessment

The PR adds approximately 5,262 lines but contains only 11 Rust unit tests. It has
no meaningful Rust integration coverage for:

- non-root installs and canonical slug resolution;
- installer upgrades and hook replacement;
- Graphiti timeout and cancellation;
- exact compatibility-worker response parity;
- authenticated model/Qdrant/Neo4j services;
- multiple stores containing the same filename;
- temporal and supersession transitions;
- partial graph-sync failures and retries;
- deleted or renamed memories;
- embedding-model migrations;
- concurrent configuration saves;
- daemon provider routing.

PR #34 currently has no CI checks. GitHub's `MERGEABLE` status means only that the
branch has no textual merge conflict; it is not evidence of correctness.

Astra ran `cargo test --workspace --locked`; all 11 tests passed across 27 test
suites. That confirms the small tested helpers but does not exercise the release
blockers above. Astra also reproduced the invalid obsolete-triple predicate with a
read-only Neo4j query and confirmed that the committed Python provider resolver
selects FastEmbed for the documented llama.cpp configuration.

## Copilot comment assessment

The 28 published inline comments are mostly valid but collapse into roughly 18
distinct issue groups. Provider routing, hard-coded root/slug defaults, missing
credentials, and unbounded Graphiti calls are reported repeatedly from different
call sites.

Two comments need severity or wording adjustment:

- The MCP config-path issue does not prevent protocol initialization or tool
  listing; it fails when recall attempts to load the wrong configuration.
- Missing hook-state TTL is real, but the files are small. Treat it as maintenance
  debt rather than an imminent disk-exhaustion blocker.

The following are deliberate behavior or deployment-specific mitigation, not proof
that the broader finding is false:

- Disabling outer keyword/vector fusion in compatibility mode was an explicit
  product decision to preserve Graphiti ordering.
- Root execution, loopback Neo4j, unauthenticated Qdrant, and the dedicated local
  BGE endpoint hide portability failures on this host.

## Additional low-priority cleanup

- Workspace crates declare MIT while the repository `LICENSE` is Apache-2.0.
- An empty chat response is reported as “no embedding.”
- The embedding recommendation catalog is duplicated in Rust and the frontend.
- The Rust “full configuration” response omits most legacy configuration fields.
- Several lifecycle and graph-sync Rust binaries still delegate to Python or shell;
  the repository is not yet a complete Rust rewrite.

## Recommended remediation order

1. Commit or deliberately separate the deployment-critical BGE-M3 changes so a
   clean checkout matches the live system.
2. Default upgrades to `graphiti_compat` and centralize config/home/slug/graph
   resolution for every binary and service.
3. Add bounded Graphiti worker execution and make the recall hook honor its enable
   switch, canonical slug, TTL, and fail-open budget.
4. Fix installer hook migration so only one recall hook runs.
5. Gate Rust indexing/native sync by supported providers, or implement complete
   provider support.
6. Add credential handling without exposing secrets through status/config APIs.
7. Fix native graph list membership, slug scoping, temporal state, supersession,
   transactional freshness, retry behavior, and stale-file reconciliation.
8. Add embedding-space fingerprints and apply query/document prefixes consistently.
9. Replace the evaluator with actual Graphiti-worker ordered fixture comparisons.
10. Fix nested metadata parsing, Atlas numeric values/save feedback, and config
    compare-and-swap behavior.
11. Add integration tests and required CI checks.
12. Request a fresh Copilot/Astra review and merge only after all release blockers
    and required checks are resolved.

## Merge recommendation

Keep the current Graphiti-compatible deployment running, but **do not merge PR #34
in its current form**. The safest path is a focused stabilization series on the same
branch, followed by a clean-install test under a non-root user and a second review.
