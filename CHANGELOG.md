# Changelog

## Unreleased — PR #34 stabilization (Rust foundation + Graphiti compatibility)

Remediation of the PR #34 review. The theme: the Rust layer was written against one
host — running as root, loopback Neo4j, unauthenticated Qdrant, one embedding
endpoint — and everything that was true there and false elsewhere was hard-coded.

### Fixed — portability
- **One resolver for config, home, graph dir and slug** (new `engram-paths` crate).
  Twenty-odd sites hard-coded `/root/.claude` or the `-root` memory slug, so any
  other user silently got the wrong config, graph environment and an *empty* memory
  store — indistinguishable from a store with no matches. Precedence mirrors
  `bin/memory_recall.py::resolve_slug` exactly, so the Python writer and the Rust
  reader can never disagree about which store they mean. CI asserts no shipped
  binary contains either literal.
- **A missing `graph:` block now defaults to `graphiti_compat`,** on the read *and*
  write sides. It defaulted to `native` in Rust, and the installer never adds the
  block, so upgrading silently moved users off their populated Graphiti index.
- **`ENGRAM_GRAPH_BACKEND` is honoured by the daemon,** not only by recall. The
  reader and the scheduled writer could target different indexes.
- **The installer migrates the recall hook instead of appending it.** An install
  that registered the Python hook and later gained the Rust binary ran *both*, with
  separate dedup state. `replace_hook` removes the sibling in one atomic rewrite.
- **Rust indexing is gated to providers it implements.** `engram-index` speaks only
  OpenAI-compatible `/v1/embeddings`, but the daemon and save hook preferred it
  whenever the binary existed — so Ollama and FastEmbed installs had indexing routed
  to a binary that cannot do it. Those configurations stay on the Python path.

### Fixed — correctness
- **Obsolete native triples are actually retired.** The predicate used Cypher's
  *string* `CONTAINS` against a list, which evaluates to `null`, so nothing was ever
  selected for retirement. Every Cypher statement now lives in a named constant with
  tests asserting its shape.
- **Native graph identity includes the store slug.** Nodes were keyed on bare
  filename, which is unique only *within* a store, so two projects sharing a
  filename merged. Requires a native rebuild.
- **Past states stop being presented as current.** Triples marked `formerly` were
  stored active with no `valid_until`, and no recall predicate read `temporal`. The
  tense heuristic also scanned the whole memory body, so one historical sentence
  retired every claim in that memory; it now reads the triple. Supersession
  identifies the prior claim by the superseded *object*.
- **Native sync no longer marks an incomplete memory current.** The SHA was
  stamped before extraction, embeddings and triples had committed, and embedding
  failures were discarded — a transient outage produced a permanently incomplete
  index that nothing revisited. The commit marker is now the last write and is
  cleared when a generation opens, so failed memories stay retryable. This is not
  full transactionality: the individual writes are still separate, and recall can
  see partially replaced facts between a failure and its retry. See *Known
  limitations*.
- **Deleted and renamed memories are pruned** from the native graph.
- **Nested `metadata.type` frontmatter is parsed.** The reader skipped indented
  lines, so every memory was typed `reference` and Qdrant type filters were wrong
  for the whole store.
- **Embedding space is part of index freshness.** Switching to a different model of
  the same dimension left every stored vector looking current; `query_prefix` and
  `document_prefix` are now applied (query side on recall, document side on index)
  instead of being parsed and ignored.
- **A configured embedding provider never falls back.** A dead llama-server used to
  answer from CPU FastEmbed — a different model in a different dimension — with no
  error anywhere. Dimension mismatches are now an error at the call site.
- **Config saves are serialized.** The revision check, validate and rename were not
  atomic as a group; writes used a fixed temp path and second-granularity backups.
  Now: an exclusive lock, a revision re-check under it, unique private temp and
  backup files, preserved mode, fsync of file and directory.

### Fixed — safety
- **Credentials are carried and never serialized.** `llama_cpp.api_key`,
  `embed.api_key`, `vector_store.api_key` and the Neo4j user/password were dropped
  by the Rust config model, so authenticated endpoints and Qdrant Cloud got
  unauthenticated requests. They are now modelled behind a `Secret` type that
  serializes as empty, so `/api/v1/config` and `/api/v1/status` cannot leak them.
- **One secret detector.** Rust used a one-line `key: value` regex, recompiled per
  memory, that matched none of the credential classes this fleet handles — bearer
  tokens, PEM keys, mnemonics, macaroons, WIF/xprv, vendor-prefixed tokens. The
  Python pattern is ported verbatim into `engram-secrets`, and both implementations
  are tested against one shared corpus. Native graph extraction, which sent raw
  memory bodies to the reasoning endpoint and to Neo4j with no redaction at all, now
  redacts first.
- **A stalled Graphiti can no longer hang a prompt.** It was a blocking
  `Command::output()` with no deadline, called from an async handler *and* from
  `UserPromptSubmit`. Now a `tokio` child with a bounded deadline, killed on expiry,
  with its stderr reported instead of discarded.
- **`local_enabled`** — the documented master kill-switch — is honoured by the Rust
  index and sync paths, which previously checked only `vector_store.enabled`.

### Changed
- The recall hook runs in **fast mode** (local BM25 + vector + one graph fact
  query). The full path measures ~3.5s against a populated graph, past the 2.5s
  per-prompt budget, so a hook on it would inject nothing. `recall.timeout_ms`
  (default 15s) bounds the Graphiti child; `recall.inject.timeout_ms` (2.5s) remains
  the per-prompt budget.
- The hook now honours `recall.inject.enabled`, `k` and `max_facts` (all previously
  hard-coded) and expires session state after 7 days, matching the Python hook.
- Recall results carry **per-memory facts** again, instead of one flattened global
  list that lost all attribution.
- Failed graph legs report *why*. Native errors were swallowed with
  `unwrap_or_default()` and the leg still reported `ok`, so an auth or schema failure
  looked like a healthy empty result. A hit whose file is missing from the store is
  now reported too — that silence was how a wrong slug looked like "no memories".
- `/api/v1/config` serves the real file through the redactor, so the
  "Full configuration / Secrets redacted" panel is both.
- Remote Neo4j is supported via `graph.neo4j_http_url`; the old error promised an
  HTTPS endpoint no code path accepted. Plaintext HTTP to a non-loopback host is
  refused.
- Atlas: numeric config fields accept the strings a browser `<input type="number">`
  actually sends (editing `dim` or `timeout_seconds` used to 422 the save); the
  save-result and reindex-warning banners survive the reload that erased them; model
  health reports reachability only when a probe actually ran; the duplicated
  embedder catalog is served from the API.
- Documentation describes compatibility mode accurately: it disables the outer
  keyword/vector legs to preserve Graphiti's ordering, and is not hybrid fusion.
- Workspace crates declare Apache-2.0, matching `LICENSE`.

### Removed
- `engram-graph-compat`, an unused second compatibility implementation that turned
  every failure into an empty success. The bounded spawn in `engram-hybrid` is the
  single path.

### Testing
- `tests/run_all.sh` runs the whole suite (Rust + Python + shell); there was no
  single entry point before. GitHub Actions (`.github/workflows/ci.yml`) runs fmt,
  clippy `-D warnings`, `cargo test --locked`, the Python and shell suites, and the
  no-host-specific-defaults check. The repository previously had no CI.
- Rust tests: 11 → 73, covering path/slug precedence, the secret corpus, nested
  frontmatter, Cypher shape, embedding-space fingerprints, secret non-serialization,
  provider gating, temporal/supersession semantics, the Graphiti deadline, and
  ordered (not set-based) recall parity.
- The parity evaluator compares the **real** compatibility path in order. It
  previously compared a raw full-text query against native recall as `HashSet`s, so
  it measured set overlap and could pass while the ordering — the only thing
  compatibility mode exists to preserve — was wrong.

### Fixed — second review pass

A follow-up review found that several of the fixes above were incomplete. What a
fix *looks* like and what it *does* are different claims, and these were the gap.

- **Every field sent off-box is redacted, not just the body.** `engram-index`
  scrubbed `body` and then interpolated `name` and `description` raw into the
  embedding request — and stored the raw description in the Qdrant payload. A
  secret in frontmatter crossed the boundary twice. The Python indexer did not
  redact at all.
- **The last unscoped native query is scoped.** `NATIVE_KEYWORD_LEGACY_INDEX` had
  no `slug` predicate and short-circuited the scoped path when it matched, so the
  cross-store leak that every other statement was fixed for stayed open through
  that one. The slug test now scans the source for statements touching `Engram*`
  labels instead of enumerating a hand-written list — which is how the omission
  survived in the first place.
- **A failed fact extraction fails.** A timeout, HTTP error or malformed JSON all
  collapsed into a heuristic facts-only result with zero triples, which the sync
  then stamped `current` — one blip of reasoning downtime silently erased a
  memory's triples and marked the result final.
- **Opening a sync generation clears the commit marker.** Moving the stamp to the
  end was not enough: an interrupted run left new content and new facts beside the
  *previous* generation's SHA, so reverting the file to that prior content made the
  freshness check match and the half-written generation was skipped forever.
- **Supersession reaches other memories.** It was restricted to prior claims in
  the same file, and a replacement almost never lives in the same file as the claim
  it replaces, so the common case did nothing.
- **Native sync is gated on reasoning *and* embeddings.** Checking only embeddings
  declared a `backend: claude`/`ollama` install eligible and then extracted against
  `llama_cpp.url` regardless.
- **URLs may not carry credentials.** `https://user:pass@host/v1` passed validation
  and was then published verbatim by `/api/v1/status` and the model editor, routing
  a password around the `Secret` type.
- **The config lock is owned by a token.** Breaking a stale lock by path alone
  raced with its own cure: the breaker could delete a *fresh* lock taken in the
  interim and put two writers in the critical section. The retry budget also gave
  up after one second against a thirty-second stale window.
- **The Python vector path got the fixes the Rust one did** — `document_prefix` and
  `query_prefix` are applied per side, and freshness includes the embedding-space
  fingerprint. These are the installs the provider gating deliberately *routes to
  Python*, so leaving them out undid the fix for everyone it applied to. The two
  fingerprint implementations are pinned equal by a literal asserted from both
  sides.

### Fixed — third review pass

Two of the second-pass fixes were themselves wrong, and the review found a further
set. The pattern worth naming: three of these were *over-corrections* — a fix that
overshot the bug.

- **The config lock is a kernel lock (`flock`), not a lockfile convention.**
  O_EXCL-plus-stale-breaking raced with its own cure, and adding an owner token
  only narrowed the window — read-then-unlink is two syscalls with nothing
  atomic between them, in acquire and in `Drop` alike. `flock` has no such
  window: ownership lives in the kernel, release is automatic on crash (so the
  staleness heuristic is gone entirely), and the lockfile is never unlinked, so
  no process can delete a lock another process holds.
- **Supersession is ordered by the source file's mtime.** Three bounds were tried.
  Restricting to one file missed the common case; removing the bound let an
  extraction retire its own siblings (triples are written immediately before);
  and ordering by the node timestamps looked like the fix but was not — those are
  *ingestion* times, so whichever memory the sync reached first won and a
  supersession would silently fail depending on filename order. `source_mtime` is
  the operator's own ordering, stable across sync order and re-indexing. Because
  mtime is whole seconds, the filename breaks ties: a plain `<=` made ties
  symmetric, so two memories saved in one editor pass could each retire the
  other's claims. `source_mtime` is deliberately *not* part of the freshness hash
  (a `touch` must not cost a re-extraction), so a separate cheap write keeps it
  current on memories the freshness check skips — without it the key only ever
  landed on memories whose content changed, and every older node compared against
  zero.
- **The legacy-edge fulltext fast path is removed, not patched.** A Neo4j fulltext
  index cannot be partitioned: it ranked across every store and applied its limit
  *before* any slug filter, so other projects' edges crowded out this store's
  hits — and because one surviving hit short-circuited the scoped query, recall
  returned an under-filled result rather than the correct one. Over-fetching to
  compensate made it several times costlier without making it correct. Abandoned
  `EngramLegacyEdge` nodes from the old import are now deleted (batched and
  capped), since they otherwise sat in the index forever.
- **Removing it required two fixes to keep recall whole,** which the first attempt
  missed while claiming "nothing is lost": the legacy fact import no longer
  requires `fact_embedding IS NOT NULL` (the old edge cache carried unembedded
  facts, and those would have been lost outright), and the keyword query no longer
  hard-filters on a match in the memory's own text before examining its facts — a
  memory whose only match was in an extracted fact or triple, exactly what the
  graph adds over text search, was being discarded. `legacy_name` is searched too.
  The trade is that the keyword leg now scans each memory's facts and triples
  instead of pruning by memory text first, which costs more on a large store.
- **`--import-legacy-embeddings` is no longer gated on the model providers.** It
  runs pure Cypher, but the provider checks sat ahead of it, so the migration this
  release documents was unreachable on precisely the read-only native install that
  most needs it.
- **`embed()` applies no prefix unless asked.** Defaulting to the document side
  would have prefixed queries too — Graphiti's embedder and the reranker in
  `graph/mg_config.py` route passages and queries through one call, and
  `memory_ai.ollama_embed` discards its role argument. A wrong prefix is worse
  than none: it moves the query out of the index's space.
- **Provider resolution is case-insensitive, matching Python.** `provider: OpenAI`
  — a spelling nothing rejects — resolved to `llama_cpp` in Python and
  `fastembed` in Rust: two embedding spaces, two fingerprints, each engine
  treating the other's records as foreign.
- **The native-sync gate asks for `llama_cpp.url`, not for `backend`.** Requiring
  `backend: llama_cpp`/`openai` as well was simply wrong: `backend` selects the
  *Python pipeline's* generation backend, while the Rust sync has always built its
  reasoning client from `llama_cpp.url` directly. An ordinary `backend: ollama`
  install with an OpenAI-compatible endpoint alongside it was working, and that
  gate declared it unserviceable.
- **An unwritable native index is reported by the daemon, not rejected at config
  load.** A `validate()` check for it was added and then removed: `validate()`
  runs on every load, so it did not warn — it made the config unloadable for every
  Rust consumer, including read-only recall against an already-populated native
  index, the parity evaluator, and `ENGRAM_GRAPH_BACKEND=graphiti_compat`, which
  could no longer rescue the install because validation runs before the override is
  read. Wrong severity for a condition that only affects the nightly writer.
- **The memory `name` is redacted before reaching Neo4j.** It is frontmatter like
  the description, it is written by the upsert, and a remote Neo4j is now
  supported — so a credential there crossed the network.
- **The config lockfile is group/other-writable,** with the mode set explicitly
  after creation. It holds no content — all the state is the kernel's — and it is
  persistent, so a restrictive mode outlives its creator: after the config changes
  hands to a service user, that user can own the config and the directory and
  still not be able to open the old owner's lockfile. `OpenOptions::mode` alone
  did not fix this, because the usual `0022` umask turns `0666` into `0644`.
- **The daemon's provider gate compares the backend case-insensitively,** like the
  two implementations it mirrors. `backend: Ollama` resolved to Ollama in both and
  to FastEmbed in the gate.

### Fixed — found by the non-root install test

The review asked for a clean install under an unprivileged user. It was worth it:
the portability work above was all correct, and two bugs it could not have caught
turned up immediately.

- **Recall worked out of the box again on a `--no-graph` install.** Two
  individually correct decisions combined into a broken one: `graphiti_compat` is
  the default so an upgraded install is never moved off its populated index, and a
  compat failure is fatal so recall can never silently reorder. On an install that
  never had the graph, that meant every query died with
  `ModuleNotFoundError: graphiti_core`. An absent Graphiti is not a failure to
  preserve ordering — there is no ordering to preserve — so it now degrades to
  local keyword + vector recall and says so in `legs`. An install that *has*
  Graphiti and then fails still fails closed. The signal is the **venv**, not the
  script: `install.sh` copies `graph/*.py` unconditionally and only builds
  `graph/venv` under `--graph`.
- **`--slug` accepts a value starting with `-`.** Every engram slug does (it is a
  path with the separators replaced), so clap read `--slug -home-alice` as a
  missing value followed by an unknown flag, and the documented invocation was
  unusable as typed.

### Fixed — second Copilot review

The fresh review resolved 24 of its earlier findings and raised five more. All
five were real, and two of them were holes left by this remediation itself.

- **Recall filters Qdrant by embedding space, not only by slug.** The `space`
  payload was being written and never read back. Indexing is incremental, so a
  same-dimension model change leaves points from two spaces in one collection —
  and vectors from different models are not comparable, so scoring a new query
  against old points produces confident nonsense with nothing to signal it. Fixed
  on both the Rust and Python search paths, and in the Python near-duplicate
  finder, where a cosine across two models is not a similarity at all. Recall
  returns fewer results until a reindex completes, which is the right trade:
  incomplete beats wrongly ranked.
- **The daemon no longer runs a Graphiti export or reconcile under a native
  reader.** `engram-graph-sync` is a wrapper around `graph/graph_sync.py`, so both
  branches of those jobs wrote the legacy schema — and the condition was inverted,
  reaching for the wrapper precisely when the backend was `native`. The same split
  brain `task_graph` was fixed for, left in place on the other two jobs.
- **The Atlas fallback defaults a missing `graph:` block to `graphiti_compat`.** It
  fabricated `native`, so saving any unrelated setting through the fallback
  materialised a block that moved an upgraded install off its populated index —
  the bug the Rust default was changed to fix, reintroduced on the path taken when
  the Rust API is down.
- **The Atlas fallback rejects credential-bearing URLs,** like Rust's `endpoint()`.
  Accepting them only when the Rust API is down is worse than accepting them
  everywhere: the bypass appears exactly when nobody is looking.
- **The recall hook resolves its slug from the payload `cwd`,** as the Python hook
  always has, rather than from the hook process's own working directory — which is
  whatever the harness launched it in. The two resolved different stores, and a
  store that does not exist injects nothing, silently, with exit 0, looking exactly
  like "no relevant memories".

### Fixed — found by deploying to a live host

- **`install.sh` refreshes the `engram-app` copy a systemd unit actually runs.**
  There are two parallel unit sets: the installer writes *user* units pointing at
  `~/.claude/rust`, but a host may run *system* units whose `ExecStart` names a
  path the installer has never written to. On the deployment that surfaced this,
  the install reported success, the binary landed, `systemctl restart` returned
  cleanly — and the API served a three-week-old build with nothing indicating it.
  With SELinux enforcing this is not a misconfiguration to correct: systemd cannot
  exec out of `/root/.claude` at all, so an enforcing host *must* run the API from
  a `bin_t` path. The installer now reads `ExecStart` from every engram unit it can
  find, refreshes any `engram-app` it names, runs `restorecon`, and reports what it
  touched. Only existing paths are refreshed — creating one would add a
  system-wide install to hosts that never asked for one.
- **Binding the API port fails with an explanation rather than a panic.** A busy
  port surfaced as `Os { code: 98 }` from `unwrap()`, which reads like a crash
  instead of "something else is already listening".
- **`test_daemon_stamp`'s compatibility check no longer depends on host state.** It
  asserted that `graph_sync.py` appeared in the command, so it passed for weeks and
  broke the moment a real install added the `engram-graph-sync` wrapper. It now
  covers both cases and asserts the invariant: the Graphiti path is used and the
  native writer is not.

### Known limitations

Stated rather than silently carried:

- **Native fact replacement is not transactional.** Facts are replaced before
  embeddings are computed, and recall does not require a current commit marker, so
  a run that fails mid-way leaves recall able to see partially replaced facts until
  the retry succeeds. Pre-existing; a proper fix is staged writes with an atomic
  marker switch.
- **Rust and Python compute the Qdrant freshness SHA over different inputs** (the
  embedding input vs the raw file bytes), so switching an install between the two
  engines re-indexes the store once. Harmless, and arguably correct given the
  engines construct their embedding input differently.
- **Filenames and types are not redacted** before going to Qdrant: the filename is
  the record's identity, and redacting it would break lookup and pruning. A secret
  in a *filename* is not covered.
- **The embedding-space fingerprint can differ between Rust and Python for a
  config neither shares.** `memory_ai.load()` merges a default `llama_cpp.url`
  that Rust's config model does not have, so a config with no explicit endpoint
  fingerprints differently on each side. Left alone deliberately: for the two to
  collide they must write the same Qdrant collection, which requires
  `provider: llama_cpp` with a non-empty endpoint — and in that case Rust either
  reads the same explicit URL or refuses to load at all. Reachable only if that
  validation changes.
- **`flock` exclusion is proven in-process.** `flock` locks are per open file
  description, so the test's probe conflicts exactly as another process's would,
  but it is a demonstration of the mechanism rather than a second `execve`.
- **Supersession is applied during the superseding memory's own sync.** A prior
  claim that first enters the graph *after* that sync is not retroactively
  retired, because nothing re-runs the earlier assertion. Re-syncing the
  superseding memory (edit it, or clear its commit marker) applies it; an
  incremental run over the new memory alone does not. There is no `--rebuild` flag
  on `engram-native-graph-sync`.

### Changed — operational notes for this release

- **A native backend that Rust cannot serve now skips the graph sync** instead of
  falling back to the Graphiti writer. Writing the index nothing is reading
  presented as memories that saved fine and then could not be recalled. Set
  `graph.backend: graphiti_compat` to use the Python writer deliberately.
- **Re-run `engram-native-graph-sync --import-legacy-embeddings`** if you use the
  native backend — recommended rather than optional now, since it is what carries
  legacy facts (including unembedded ones) into the slug-scoped nodes that
  replaced the removed fulltext path. It now also deletes the `EngramLegacyEdge`
  cache the previous version created — those nodes are no longer read by anything
  and otherwise stay in the fulltext index forever. Until it is re-run, recall sees
  fewer legacy facts than before: the import is what carries them into the
  slug-scoped `EngramFact` nodes that the removed fulltext path used to serve.
- **`graph.backend: native` without an OpenAI-compatible `llama_cpp.url` and
  embedding endpoint** logs a skip from the daemon's graph job each run rather
  than writing anything. The config still loads (recall against an existing native
  index keeps working); set `graph.backend: graphiti_compat` or configure both
  endpoints.
- **The first Python vector sync after upgrading re-indexes the whole store,**
  because the freshness key now includes the embedding space. Expected once.
- **A URL with embedded credentials is now a config error.** Move it to the
  matching `api_key`.

## 1.1.0 — auto-recall, the console, and a graph that actually inserts

### Added
- **Auto-recall on every prompt.** `hooks/memory-recall-inject.py` (a `UserPromptSubmit`
  hook merged by the installer) injects the memories matching each prompt, so recall no
  longer depends on Claude choosing to call a recall tool. Names + descriptions only,
  **deduped per session** (a memory or fact is injected at most once per session, state
  under `~/.claude/logs/recall-inject/`), fail-open, ~0.3s. Configure under
  `recall.inject`; `ENGRAM_HOOK_DEBUG=1` explains any no-op.
- **`bin/memory_recall.py`** — one hybrid-recall implementation (RRF over graph +
  vector + keyword) shared by the hook, the console, and the `memory_recall_hybrid`
  MCP tool, which previously carried its own copy. Also a CLI:
  `memory_recall.py "<query>" --k 6 [--fast] [--json]`.
- **`bin/engram-tui.py`** — the engram console: dashboard, memories, recall, vector,
  graph, skills and queues in a stdlib `curses` UI. Mutations still go through
  `save_memory.sh` / `delete_memory.sh`.

### Removed
- **The GUI** (`bin/engram_api.py`, `bin/engram-ui.sh`, `ui/index.html`) — replaced by
  the console. It required FastAPI + uvicorn and a running server; `install.sh` and
  `uninstall.sh` clean up the old files.

### Changed
- Recall legs talk to Qdrant / Ollama / Neo4j over **HTTP instead of their client
  libraries** (`import qdrant_client` alone measured 0.78s against a 0.04s search), so
  the hook and console run on system `python3` with no venv: 1.85s → 0.3s.

### Fixed — the graph insert had been silently producing nothing
The nightly graph insert produced **zero rows for 7 consecutive nights** while every
timer stayed green. Three instances of one defect class — a single malformed record
aborting an entire run:
- A blank edge `fact` embeds to `[]` and Neo4j's `setRelationshipVectorProperty`
  rejects an empty vector: **1 factless edge out of 170 killed all 10 memories** in
  the batch. The fact is now synthesized from the triple instead of dropped.
- An entity with no `name` raised `KeyError` in the global type pre-pass, which runs
  *before* any insert — **4 nameless entities in one JSON blocked all 142 pending
  memories**. Guarded in both the pre-pass and the per-memory loop.
- An unbounded run is killed by the daemon's 3600s cap mid-extraction and commits
  nothing, so `task_graph` now passes `--limit`.

- **Extraction retries that can actually work.** The model can fall into a degenerate
  non-JSON reply and repeat it *byte-identically* (measured 3/3 at temperature 0.2), so
  retrying the same call is useless. `extract()` escalates sampling instead
  (`None → 0.6 → 1.0`). Stays on the local backend; no fallback provider.
- **`MENTIONS` are created on insert.** The insert built Entity nodes and Entity→Entity
  `RELATES_TO` but never linked the episode to what it mentions, leaving
  "which memories mention X" untraversable for **372 of 613 episodes**.
- **`--rebuild` no longer duplicates the graph.** It resets the local state file and
  deletes nothing in Neo4j, so on a populated graph it minted a second episode per
  memory — the opposite of what its own docstring claimed, and the source of 126
  duplicates. It now refuses a non-empty graph without `--force`.
- **Timeouts stop reporting success.** A timed-out task no longer stamps
  `daemon_state.json` as a completed run, and `maintenance` skips `memory_pipeline.sh`
  when `harvest` already ran it — without a dedicated fixate script both tasks ran the
  same pipeline twice a night.
- `MEMORY_FULL.md` is excluded from the store scan, so the index is not extracted as a
  memory.

### Added
- **`graph/graph_maint.py`** — repairs the sync path can't do itself: `--dedup`,
  `--prune-stale`, `--refresh-changed`, `--backfill-mentions`, `--prune-isolated`,
  `--all`. Dry-run by default. Every selection is `group_id`-scoped so a shared Neo4j is
  never touched; staleness is judged against `Episodic.source_md` rather than
  `sync_state` (bootstrap-era memories have no `sync_state` entry, which made the
  "293 changed memory(ies)" banner false — only 56 were real).
- **`install.sh` starts and verifies its dependencies.** It now runs `docker compose up -d`
  for Neo4j/Qdrant instead of printing the command, then checks the container restart
  policy and that docker is enabled at boot. Both compose files already declared
  `restart: unless-stopped`; nothing ever ran them. `--no-start-services` opts out.


## 1.0.0 — The autonomous release

engram now runs the **full memory lifecycle unattended** — encode, graduate,
consolidate, promote, prune — behind a **reversibility-tiered** safety model with a
**one-tap Telegram approval gate** for the few irreversible ops. **Zero slash commands
required.**

### Headline
- **Zero-command, fully autonomous.** Harvest → graduate → fixate → consolidate →
  promote all run on the 24/7 daemon. The curation/promotion commands are retired; ask
  in plain English (*"tidy my memories"*, *"make this a skill"*) for on-demand.
- **Reversibility-tiered safety.** Reversible/deterministic ops run silently; reversible
  judgment ops apply with a one-tap **UNDO**; irreversible/behavioral ops (skill installs,
  lossy merges, orphan prunes, permanent deletes) require a one-tap **approval** on
  Telegram — 72h TTL then **drop** (never default-apply).
- **Headless generation via cc-gateway (`ccg`).** Works from a systemd daemon with no
  interactive login; `fallback: claude` for hosts not behind a gateway.
- **Codex is optional.** The risky-op reviewer uses Codex if installed, else falls back
  to a human Telegram approval — most users run only Claude.

### Autonomy
- Async approval queue + Telegram gate: long-poll (no webhook/public endpoint),
  file-queue with atomic-rename state machine, opaque replay-proof callback ids,
  chat-id allowlist, artifact-hash-bound skill installs, weekly digest, probation sweep.
- **Compress-then-quarantine merges** — losslessness via *reversibility* (sources →
  `.quarantine/`, 30-day probation, one-tap undo), transaction-scoped backups,
  failure-atomic + freshness-checked undo.
- **Suspect lifecycle** — injection suspects auto-quarantine → Telegram RESTORE → auto-purge.
- **Orphan-prune proposer** and **explicit skill-promotion intent** (`promote: requested`).
- **Activity log** — a Telegram summary of every unattended run.
- **Cadences tuned to each stage's time-constant**: harvest hourly (idle-grace = only
  *finished* chats), fixate nightly, distill weekly, consolidate weekly.

### Safety hardening
- Centralized secret scanner (bearer/PEM/macaroon/BIP39/seed/WIF) on **every** writer,
  before the GitHub push, and before any text reaches an LLM.
- Harvest data-loss fixes: watermark advances only past harvested segments; holds on a
  garbage LLM response; partial-line safe.
- Injection: fail-closed deterministic denylist + `<system-reminder>` stripping at
  harvest (closes the self-poisoning loop); injection-resistant verdict parsing.
- Deterministic `MEMORY.md` index generation (no silent orphans, stays under the load limit).

### Recall
- Vector embeddings now include the memory **body** (was title-only).
- `memory_recall` consolidated to the hybrid ranker; deleted/renamed-memory poisoning fixed.

### Upgrading
`install.sh` is the idempotent updater — it preserves your `engram.yaml` and `daemon.env`
secrets, re-registers MCP, and surfaces new config keys. See **README → Updating** and
**AUTONOMY.md**.
