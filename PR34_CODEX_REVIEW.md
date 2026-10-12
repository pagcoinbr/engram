1. Native sync mutates graph data while retaining the previous “current” stamp. An embedding failure therefore does not leave the memory un-stamped as required.
2. Native keyword recall still returns results from an unscoped legacy index, reopening cross-store leakage.
3. Rust indexing redacts only the body; names and descriptions are sent to the embedding endpoint and Qdrant unredacted.
4. Rust store location can diverge from the Python writer when `ENGRAM_CONFIG` is outside the Engram home.
5. `ConfigLock` has stale-lock unlink races and can permit two simultaneous writers.
6. Native sync gates only the embedding provider, then unconditionally uses `llama_cpp` for reasoning.

## 13-blocker closure

| # | Status | Assessment |
|---|---|---|
| 1 | CLOSED | The Python provider resolver now supports `llama_cpp`/OpenAI-compatible embeddings, rejects dimension mismatch, and does not silently fall back for an explicitly selected provider at [bin/engram_llm.py](/root/ASCP/engram/bin/engram_llm.py:354), [bin/engram_llm.py](/root/ASCP/engram/bin/engram_llm.py:383), and [bin/engram_llm.py](/root/ASCP/engram/bin/engram_llm.py:427). Fast recall uses the same endpoint and bearer token at [bin/memory_recall.py](/root/ASCP/engram/bin/memory_recall.py:143). |
| 2 | CLOSED | `NOT obsolete.key IN [triple IN $triples \| triple.key]` at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:267) is valid Cypher: `IN` performs list membership and `NOT` negates that boolean result. No remaining statement applies `CONTAINS` to a list; remaining uses at lines 179, 206, 209, and 212 compare strings. The regression assertion is at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:813). |
| 3 | PARTIAL | The shared Rust resolver has the intended ordering at [crates/engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:75), and hard-coded root defaults are gone. But `store_dir` anchors the store to the config file’s parent at [crates/engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:96), whereas the Python writer anchors it under `$HOME/.claude` at [bin/memory_lib.sh](/root/ASCP/engram/bin/memory_lib.sh:43). An external `ENGRAM_CONFIG` therefore makes Rust and Python address different stores. Also, installer hook commands carry neither `--config` nor `ENGRAM_BIN` at [install.sh](/root/ASCP/engram/install.sh:371), breaking `ENGRAM_CLAUDE_HOME` installs unless those variables happen to be inherited. |
| 4 | PARTIAL | The Graphiti child is now bounded by `recall.timeout_ms` with `kill_on_drop` at [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:340) and [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:371). The Rust hook uses `recall.inject.timeout_ms` at [engram-recall-hook.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-recall-hook.rs:90). However, store loading and BM25 execute synchronously before an async yield at [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:161) and [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:184), so Tokio’s timeout cannot preempt them. The Python fallback explicitly uses only per-call HTTP timeouts, not a wall-clock deadline, at [memory-recall-inject.py](/root/ASCP/engram/bin/hooks/memory-recall-inject.py:108). A prompt can still exceed its budget. |
| 5 | CLOSED | `replace_hook` removes the named sibling from nested hook arrays, preserves unrelated entries, drops empty wrappers, and adds the desired hook only when absent at [install.sh](/root/ASCP/engram/install.sh:348). The upgrade, rollback, idempotence, broken-double-state, and unrelated-hook cases are asserted at [tests/test_install_hook_migration.sh](/root/ASCP/engram/tests/test_install_hook_migration.sh:34). A pre-existing duplicate of the desired command is not normalized, but the reported Python-plus-Rust upgrade failure is closed. |
| 6 | PARTIAL | Vector indexing is properly gated in Rust at [engram-index.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-index.rs:72), in the daemon at [daemon/engram-daemon.py](/root/ASCP/engram/daemon/engram-daemon.py:268), and in the save hook at [bin/memory_lib.sh](/root/ASCP/engram/bin/memory_lib.sh:263). Native sync is not fully gated: it checks only embedding support at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:97), then unconditionally creates its reasoning client from `llama_cpp.url` at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:135). A Claude/Ollama reasoning configuration plus an OpenAI-compatible embedder is declared eligible and then fails or uses the wrong reasoning backend. The daemon’s “Python fallback” at [daemon/engram-daemon.py](/root/ASCP/engram/daemon/engram-daemon.py:252) writes Graphiti data while the configured reader remains native. |
| 7 | NOT CLOSED | The final stamp was moved to the end at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:293), and embedding errors propagate at line 282. But the initial upsert does not clear the old `sha`, `embedding_space`, or version marker at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:224). Facts are replaced before embeddings at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:260). Thus an embedding failure leaves partially updated facts beside the previous “current” stamp. If the file is reverted to the prior content before retry, the old SHA matches and the corrupted generation is skipped. Additionally, reasoning timeout/error/invalid JSON is converted into a successful facts-only extraction and then stamped current at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:237). |
| 8 | NOT CLOSED | Most native statements now scope memory, fact, triple, freshness, semantic recall, and reconciliation by slug. However, `NATIVE_KEYWORD_LEGACY_INDEX` has no slug predicate at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:198), and `native_keyword_files` returns those hits immediately at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:422). Legacy edge creation is likewise unscoped at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:614). The “every native statement” test omits `NATIVE_KEYWORD_LEGACY_INDEX` at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:828), which is why tests pass despite the leak. |
| 9 | PARTIAL | `formerly` is now historical/closed at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:54), and active recall checks both status and `valid_until`. Supersession targets the superseded object at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:103). But application is restricted to prior claims attached to the same memory file at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:287). A replacement asserted in one memory cannot retire a prior claim in another memory in the same slug. Tests cover only target calculation and query substrings, not transitions across memories, at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:891). |
| 10 | PARTIAL | Compatibility output now retains per-record facts and neighbours, and Graphiti execution has explicit failure/timeout semantics. The evaluator invokes the actual compatibility and native paths and preserves ordering at [engram-graph-recall-eval.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-graph-recall-eval.rs:80). It explicitly admits it is not a frozen-fixture parity suite at line 12. `--strict` checks only mean file-prefix agreement at lines 95 and 130; fact loss is merely printed, and neighbours, duplicate episodes, ties, metadata, concurrency, and frozen error/timeout cases are not compared. `Output` still has no engine/index version metadata at [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:30). |
| 11 | PARTIAL | API keys are carried to model requests, Qdrant default headers, and Neo4j credentials; direct `Secret` serialization and `Debug` are safe at [secret.rs](/root/ASCP/engram/crates/engram-config/src/secret.rs:48) and [secret.rs](/root/ASCP/engram/crates/engram-config/src/secret.rs:59). Public serialization is not completely safe. Endpoint validation permits URL userinfo at [engram-config/src/lib.rs](/root/ASCP/engram/crates/engram-config/src/lib.rs:545), profiles retain that URL verbatim at lines 526–538, and `/api/v1/status` returns it at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:598). A URL such as `https://user:password@host/v1` exposes its credential. `/api/v1/config` also serializes the raw YAML after regex redaction at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:536); the detector requires six characters for named values and is not a structural guarantee at [engram-secrets/src/lib.rs](/root/ASCP/engram/crates/engram-secrets/src/lib.rs:32). |
| 12 | CLOSED | Missing `graph` configuration defaults to `graphiti_compat` at [engram-config/src/lib.rs](/root/ASCP/engram/crates/engram-config/src/lib.rs:105), with a regression test at line 585. The daemon uses the same default and honors the environment override at [daemon/engram-daemon.py](/root/ASCP/engram/daemon/engram-daemon.py:86). |
| 13 | NOT CLOSED | Native extraction redacts description/body before the LLM and before the Neo4j memory write at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:222) and lines 260–267. Rust vector indexing, however, redacts only `memory.body`; raw `memory.name` and `memory.description` are included in the off-box embedding request at [engram-index.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-index.rs:111). The raw description is then written into Qdrant at line 151. A secret in imported or hand-edited frontmatter still crosses the boundary. |

## Requested regression checks

### Path and slug precedence

The returned slug ordering is correct for normal non-empty values: explicit Rust CLI value, environment, `engram.env`, optional cwd, then home at [engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:75). It is not behaviorally identical end-to-end:

- The Rust hook derives cwd from the process at [engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:70); the Python hook uses `cwd` from the hook payload at [memory-recall-inject.py](/root/ASCP/engram/bin/hooks/memory-recall-inject.py:112).
- Rust anchors the store to the config directory; Python writers anchor it to `$HOME/.claude`.
- Alternate-home hook registration does not pass the resolved config/home.

Those divergences can present exactly as silent missing memories.

### Timeout split

The 15-second Graphiti child budget and 2.5-second injection budget are assigned to the intended paths. The budget is not a hard wall-clock guarantee because both Rust and Python perform synchronous, non-preemptible local work. The Python hook’s own comment acknowledges this at [memory-recall-inject.py](/root/ASCP/engram/bin/hooks/memory-recall-inject.py:108).

### `ConfigLock`

The revision is rechecked while nominally holding the lock at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:271), and replacement is a same-directory atomic rename at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:485).

A race survives:

1. Process A observes a lock older than 30 seconds.
2. The old owner removes it and process B creates a fresh lock.
3. A executes the unconditional unlink at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:420), deleting B’s lock.
4. A creates another lock, leaving A and B in the critical section simultaneously.

A legitimate save lasting more than 30 seconds can also have its live lock broken. The lock contains no owner token, inode check, or lease refresh. A crash does not wedge it forever—the next acquisition after 30 seconds removes it—but attempts during the first 30 seconds fail after only one second of retries.

### `Secret`

The newtype itself is safe, but not every response path is. URL userinfo leaks through `/api/v1/status`, and raw-config regex masking is not equivalent to structurally removing every secret-bearing key.

### `replace_hook`

For the intended migration state, the jq filter is correct and idempotent. It removes only commands named in `drop`, preserves sibling hooks and other events, removes empty wrappers, and avoids adding a second desired command. The test genuinely drives the functions extracted from `install.sh`.

## Additional remediation regressions

- Python indexing does not apply `document_prefix` or `query_prefix`. Both document and query paths call the same unprefixed `engram_llm.embed` at [vector/vector_store.py](/root/ASCP/engram/vector/vector_store.py:84) and [vector/vector_store.py](/root/ASCP/engram/vector/vector_store.py:104). The added prefix test covers only `memory_recall.embed`, not the Python index writer.
- Python vector freshness remains content-only: `_sha` hashes only the file at [vector/vector_sync.py](/root/ASCP/engram/vector/vector_sync.py:48), and that value alone controls skipping at line 113. Ollama/FastEmbed installations routed to Python therefore still do not invalidate on a same-dimension model or prefix change.
- The documented `embed.url` fallback conflicts with Rust validation: `embed_endpoint()` supports falling back to `llama_cpp.url`, but validation rejects `llama_cpp`/`openai` whenever `embed.url` itself is empty at [engram-config/src/lib.rs](/root/ASCP/engram/crates/engram-config/src/lib.rs:434).

## Tests and CI

I ran both suites:

- Rust: 62 tests passed across 30 suites.
- Python/shell: 23 passed, 1 skipped, 0 failed. The graph-insert test skipped its dependency-backed portion because `graphiti_core` was unavailable.

What is actually asserted:

- Rust formatting, Clippy warnings, and unit tests are required by CI at [.github/workflows/ci.yml](/root/ASCP/engram/.github/workflows/ci.yml:28).
- Resolver helpers, query strings, pure lifecycle helpers, fake Graphiti child timeout/failure, secret serialization, and editor helpers have assertions.
- The install-hook shell test checks the real jq filter.
- Python tests assert provider-selection helpers, fake HTTP request construction, pure fusion behavior, and source-text guards.

What is merely executed or absent:

- No Neo4j, Qdrant, or model service is started in CI; service-dependent tests are explicitly permitted to skip at [tests/run_all.sh](/root/ASCP/engram/tests/run_all.sh:10).
- The graph parity evaluator and its case file are never invoked by CI.
- `test_graph_sync_state.py` uses source-string positioning checks, not fault injection against Neo4j.
- The native slug test omits the unscoped legacy keyword statement.
- E2E suites are disabled unless `ENGRAM_E2E=1` at [tests/run_all.sh](/root/ASCP/engram/tests/run_all.sh:77).
- Atlas has no build, lint, or UI test job despite frontend changes.

The highest-value missing test is a real Neo4j native-sync integration test that uses two slugs with the same filename, injects an embedding failure after fact replacement, and asserts both that the failed memory has no current marker and that neither keyword nor semantic recall can return the other slug’s data.

## Must change first

1. Make native sync generation-safe: clear/invalidate the prior marker before mutating, or stage all generation data and atomically switch the marker; propagate extraction failure instead of stamping a facts-only fallback.
2. Remove or slug-scope the legacy keyword fast path and legacy-edge import; add a two-slug Neo4j integration test.
3. Redact name, description, body, and outbound payload fields before embedding/Qdrant/Neo4j writes.
4. Separate Engram home from config location, pass resolved home/config/slug into installed hooks, and enforce a genuinely preemptive hook deadline.
5. Gate native sync on both reasoning and embedding providers; do not fall back to a Graphiti writer while the reader remains native.
6. Fix cross-memory supersession, structural API redaction, and the stale-lock unlink race.
7. Add live-service native integration tests and a strict frozen Graphiti parity suite to required CI.

DO NOT MERGE
tokens used
206,314
## Review result

The remediation is not merge-ready. Four release blockers remain materially open, several are only partial, and CI does not exercise the dangerous native-graph paths.

The most serious findings are:

1. Native sync mutates graph data while retaining the previous “current” stamp. An embedding failure therefore does not leave the memory un-stamped as required.
2. Native keyword recall still returns results from an unscoped legacy index, reopening cross-store leakage.
3. Rust indexing redacts only the body; names and descriptions are sent to the embedding endpoint and Qdrant unredacted.
4. Rust store location can diverge from the Python writer when `ENGRAM_CONFIG` is outside the Engram home.
5. `ConfigLock` has stale-lock unlink races and can permit two simultaneous writers.
6. Native sync gates only the embedding provider, then unconditionally uses `llama_cpp` for reasoning.

## 13-blocker closure

| # | Status | Assessment |
|---|---|---|
| 1 | CLOSED | The Python provider resolver now supports `llama_cpp`/OpenAI-compatible embeddings, rejects dimension mismatch, and does not silently fall back for an explicitly selected provider at [bin/engram_llm.py](/root/ASCP/engram/bin/engram_llm.py:354), [bin/engram_llm.py](/root/ASCP/engram/bin/engram_llm.py:383), and [bin/engram_llm.py](/root/ASCP/engram/bin/engram_llm.py:427). Fast recall uses the same endpoint and bearer token at [bin/memory_recall.py](/root/ASCP/engram/bin/memory_recall.py:143). |
| 2 | CLOSED | `NOT obsolete.key IN [triple IN $triples \| triple.key]` at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:267) is valid Cypher: `IN` performs list membership and `NOT` negates that boolean result. No remaining statement applies `CONTAINS` to a list; remaining uses at lines 179, 206, 209, and 212 compare strings. The regression assertion is at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:813). |
| 3 | PARTIAL | The shared Rust resolver has the intended ordering at [crates/engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:75), and hard-coded root defaults are gone. But `store_dir` anchors the store to the config file’s parent at [crates/engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:96), whereas the Python writer anchors it under `$HOME/.claude` at [bin/memory_lib.sh](/root/ASCP/engram/bin/memory_lib.sh:43). An external `ENGRAM_CONFIG` therefore makes Rust and Python address different stores. Also, installer hook commands carry neither `--config` nor `ENGRAM_BIN` at [install.sh](/root/ASCP/engram/install.sh:371), breaking `ENGRAM_CLAUDE_HOME` installs unless those variables happen to be inherited. |
| 4 | PARTIAL | The Graphiti child is now bounded by `recall.timeout_ms` with `kill_on_drop` at [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:340) and [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:371). The Rust hook uses `recall.inject.timeout_ms` at [engram-recall-hook.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-recall-hook.rs:90). However, store loading and BM25 execute synchronously before an async yield at [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:161) and [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:184), so Tokio’s timeout cannot preempt them. The Python fallback explicitly uses only per-call HTTP timeouts, not a wall-clock deadline, at [memory-recall-inject.py](/root/ASCP/engram/bin/hooks/memory-recall-inject.py:108). A prompt can still exceed its budget. |
| 5 | CLOSED | `replace_hook` removes the named sibling from nested hook arrays, preserves unrelated entries, drops empty wrappers, and adds the desired hook only when absent at [install.sh](/root/ASCP/engram/install.sh:348). The upgrade, rollback, idempotence, broken-double-state, and unrelated-hook cases are asserted at [tests/test_install_hook_migration.sh](/root/ASCP/engram/tests/test_install_hook_migration.sh:34). A pre-existing duplicate of the desired command is not normalized, but the reported Python-plus-Rust upgrade failure is closed. |
| 6 | PARTIAL | Vector indexing is properly gated in Rust at [engram-index.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-index.rs:72), in the daemon at [daemon/engram-daemon.py](/root/ASCP/engram/daemon/engram-daemon.py:268), and in the save hook at [bin/memory_lib.sh](/root/ASCP/engram/bin/memory_lib.sh:263). Native sync is not fully gated: it checks only embedding support at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:97), then unconditionally creates its reasoning client from `llama_cpp.url` at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:135). A Claude/Ollama reasoning configuration plus an OpenAI-compatible embedder is declared eligible and then fails or uses the wrong reasoning backend. The daemon’s “Python fallback” at [daemon/engram-daemon.py](/root/ASCP/engram/daemon/engram-daemon.py:252) writes Graphiti data while the configured reader remains native. |
| 7 | NOT CLOSED | The final stamp was moved to the end at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:293), and embedding errors propagate at line 282. But the initial upsert does not clear the old `sha`, `embedding_space`, or version marker at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:224). Facts are replaced before embeddings at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:260). Thus an embedding failure leaves partially updated facts beside the previous “current” stamp. If the file is reverted to the prior content before retry, the old SHA matches and the corrupted generation is skipped. Additionally, reasoning timeout/error/invalid JSON is converted into a successful facts-only extraction and then stamped current at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:237). |
| 8 | NOT CLOSED | Most native statements now scope memory, fact, triple, freshness, semantic recall, and reconciliation by slug. However, `NATIVE_KEYWORD_LEGACY_INDEX` has no slug predicate at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:198), and `native_keyword_files` returns those hits immediately at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:422). Legacy edge creation is likewise unscoped at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:614). The “every native statement” test omits `NATIVE_KEYWORD_LEGACY_INDEX` at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:828), which is why tests pass despite the leak. |
| 9 | PARTIAL | `formerly` is now historical/closed at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:54), and active recall checks both status and `valid_until`. Supersession targets the superseded object at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:103). But application is restricted to prior claims attached to the same memory file at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:287). A replacement asserted in one memory cannot retire a prior claim in another memory in the same slug. Tests cover only target calculation and query substrings, not transitions across memories, at [crates/engram-graph/src/lib.rs](/root/ASCP/engram/crates/engram-graph/src/lib.rs:891). |
| 10 | PARTIAL | Compatibility output now retains per-record facts and neighbours, and Graphiti execution has explicit failure/timeout semantics. The evaluator invokes the actual compatibility and native paths and preserves ordering at [engram-graph-recall-eval.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-graph-recall-eval.rs:80). It explicitly admits it is not a frozen-fixture parity suite at line 12. `--strict` checks only mean file-prefix agreement at lines 95 and 130; fact loss is merely printed, and neighbours, duplicate episodes, ties, metadata, concurrency, and frozen error/timeout cases are not compared. `Output` still has no engine/index version metadata at [crates/engram-hybrid/src/lib.rs](/root/ASCP/engram/crates/engram-hybrid/src/lib.rs:30). |
| 11 | PARTIAL | API keys are carried to model requests, Qdrant default headers, and Neo4j credentials; direct `Secret` serialization and `Debug` are safe at [secret.rs](/root/ASCP/engram/crates/engram-config/src/secret.rs:48) and [secret.rs](/root/ASCP/engram/crates/engram-config/src/secret.rs:59). Public serialization is not completely safe. Endpoint validation permits URL userinfo at [engram-config/src/lib.rs](/root/ASCP/engram/crates/engram-config/src/lib.rs:545), profiles retain that URL verbatim at lines 526–538, and `/api/v1/status` returns it at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:598). A URL such as `https://user:password@host/v1` exposes its credential. `/api/v1/config` also serializes the raw YAML after regex redaction at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:536); the detector requires six characters for named values and is not a structural guarantee at [engram-secrets/src/lib.rs](/root/ASCP/engram/crates/engram-secrets/src/lib.rs:32). |
| 12 | CLOSED | Missing `graph` configuration defaults to `graphiti_compat` at [engram-config/src/lib.rs](/root/ASCP/engram/crates/engram-config/src/lib.rs:105), with a regression test at line 585. The daemon uses the same default and honors the environment override at [daemon/engram-daemon.py](/root/ASCP/engram/daemon/engram-daemon.py:86). |
| 13 | NOT CLOSED | Native extraction redacts description/body before the LLM and before the Neo4j memory write at [engram-native-graph-sync.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-native-graph-sync.rs:222) and lines 260–267. Rust vector indexing, however, redacts only `memory.body`; raw `memory.name` and `memory.description` are included in the off-box embedding request at [engram-index.rs](/root/ASCP/engram/crates/engram-app/src/bin/engram-index.rs:111). The raw description is then written into Qdrant at line 151. A secret in imported or hand-edited frontmatter still crosses the boundary. |

## Requested regression checks

### Path and slug precedence

The returned slug ordering is correct for normal non-empty values: explicit Rust CLI value, environment, `engram.env`, optional cwd, then home at [engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:75). It is not behaviorally identical end-to-end:

- The Rust hook derives cwd from the process at [engram-paths/src/lib.rs](/root/ASCP/engram/crates/engram-paths/src/lib.rs:70); the Python hook uses `cwd` from the hook payload at [memory-recall-inject.py](/root/ASCP/engram/bin/hooks/memory-recall-inject.py:112).
- Rust anchors the store to the config directory; Python writers anchor it to `$HOME/.claude`.
- Alternate-home hook registration does not pass the resolved config/home.

Those divergences can present exactly as silent missing memories.

### Timeout split

The 15-second Graphiti child budget and 2.5-second injection budget are assigned to the intended paths. The budget is not a hard wall-clock guarantee because both Rust and Python perform synchronous, non-preemptible local work. The Python hook’s own comment acknowledges this at [memory-recall-inject.py](/root/ASCP/engram/bin/hooks/memory-recall-inject.py:108).

### `ConfigLock`

The revision is rechecked while nominally holding the lock at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:271), and replacement is a same-directory atomic rename at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:485).

A race survives:

1. Process A observes a lock older than 30 seconds.
2. The old owner removes it and process B creates a fresh lock.
3. A executes the unconditional unlink at [main.rs](/root/ASCP/engram/crates/engram-app/src/main.rs:420), deleting B’s lock.
4. A creates another lock, leaving A and B in the critical section simultaneously.

A legitimate save lasting more than 30 seconds can also have its live lock broken. The lock contains no owner token, inode check, or lease refresh. A crash does not wedge it forever—the next acquisition after 30 seconds removes it—but attempts during the first 30 seconds fail after only one second of retries.

### `Secret`

The newtype itself is safe, but not every response path is. URL userinfo leaks through `/api/v1/status`, and raw-config regex masking is not equivalent to structurally removing every secret-bearing key.

### `replace_hook`

For the intended migration state, the jq filter is correct and idempotent. It removes only commands named in `drop`, preserves sibling hooks and other events, removes empty wrappers, and avoids adding a second desired command. The test genuinely drives the functions extracted from `install.sh`.

## Additional remediation regressions

- Python indexing does not apply `document_prefix` or `query_prefix`. Both document and query paths call the same unprefixed `engram_llm.embed` at [vector/vector_store.py](/root/ASCP/engram/vector/vector_store.py:84) and [vector/vector_store.py](/root/ASCP/engram/vector/vector_store.py:104). The added prefix test covers only `memory_recall.embed`, not the Python index writer.
- Python vector freshness remains content-only: `_sha` hashes only the file at [vector/vector_sync.py](/root/ASCP/engram/vector/vector_sync.py:48), and that value alone controls skipping at line 113. Ollama/FastEmbed installations routed to Python therefore still do not invalidate on a same-dimension model or prefix change.
- The documented `embed.url` fallback conflicts with Rust validation: `embed_endpoint()` supports falling back to `llama_cpp.url`, but validation rejects `llama_cpp`/`openai` whenever `embed.url` itself is empty at [engram-config/src/lib.rs](/root/ASCP/engram/crates/engram-config/src/lib.rs:434).

## Tests and CI

I ran both suites:

- Rust: 62 tests passed across 30 suites.
- Python/shell: 23 passed, 1 skipped, 0 failed. The graph-insert test skipped its dependency-backed portion because `graphiti_core` was unavailable.

What is actually asserted:

- Rust formatting, Clippy warnings, and unit tests are required by CI at [.github/workflows/ci.yml](/root/ASCP/engram/.github/workflows/ci.yml:28).
- Resolver helpers, query strings, pure lifecycle helpers, fake Graphiti child timeout/failure, secret serialization, and editor helpers have assertions.
- The install-hook shell test checks the real jq filter.
- Python tests assert provider-selection helpers, fake HTTP request construction, pure fusion behavior, and source-text guards.

What is merely executed or absent:

- No Neo4j, Qdrant, or model service is started in CI; service-dependent tests are explicitly permitted to skip at [tests/run_all.sh](/root/ASCP/engram/tests/run_all.sh:10).
- The graph parity evaluator and its case file are never invoked by CI.
- `test_graph_sync_state.py` uses source-string positioning checks, not fault injection against Neo4j.
- The native slug test omits the unscoped legacy keyword statement.
- E2E suites are disabled unless `ENGRAM_E2E=1` at [tests/run_all.sh](/root/ASCP/engram/tests/run_all.sh:77).
- Atlas has no build, lint, or UI test job despite frontend changes.

The highest-value missing test is a real Neo4j native-sync integration test that uses two slugs with the same filename, injects an embedding failure after fact replacement, and asserts both that the failed memory has no current marker and that neither keyword nor semantic recall can return the other slug’s data.

## Must change first

1. Make native sync generation-safe: clear/invalidate the prior marker before mutating, or stage all generation data and atomically switch the marker; propagate extraction failure instead of stamping a facts-only fallback.
2. Remove or slug-scope the legacy keyword fast path and legacy-edge import; add a two-slug Neo4j integration test.
3. Redact name, description, body, and outbound payload fields before embedding/Qdrant/Neo4j writes.
4. Separate Engram home from config location, pass resolved home/config/slug into installed hooks, and enforce a genuinely preemptive hook deadline.
5. Gate native sync on both reasoning and embedding providers; do not fall back to a Graphiti writer while the reader remains native.
6. Fix cross-memory supersession, structural API redaction, and the stale-lock unlink race.
7. Add live-service native integration tests and a strict frozen Graphiti parity suite to required CI.

DO NOT MERGE

[exited with code 0]
