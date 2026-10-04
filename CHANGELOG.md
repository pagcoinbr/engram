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
- **Native sync commits transactionally.** The SHA was stamped before extraction,
  embeddings and triples had committed, and embedding failures were discarded — a
  transient outage produced a permanently incomplete index that nothing revisited.
  The commit marker is now the last write, and failed memories stay retryable.
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
- Rust tests: 11 → 60, covering path/slug precedence, the secret corpus, nested
  frontmatter, Cypher shape, embedding-space fingerprints, secret non-serialization,
  provider gating, temporal/supersession semantics, the Graphiti deadline, and
  ordered (not set-based) recall parity.
- The parity evaluator compares the **real** compatibility path in order. It
  previously compared a raw full-text query against native recall as `HashSet`s, so
  it measured set overlap and could pass while the ordering — the only thing
  compatibility mode exists to preserve — was wrong.

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
