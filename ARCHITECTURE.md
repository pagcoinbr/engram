# engram architecture

engram has two layers: a **Markdown store** (the source of truth) and a **knowledge
graph** (a synced associative index over it). Everything else — harvest, consolidation,
fixation, recall — is a pipeline modeled on human memory, with all LLM work behind one
pluggable backend.

## 1. The store (source of truth)
- Memories are `.md` files in `~/.claude/projects/<slug>/memory/`. One file = one fact.
- Frontmatter `name` / `description` / `type` (`user` | `feedback` | `project` | `reference`),
  then a body in the **Summary → numbered Index → Body** shape so a file is graspable at a glance.
- `MEMORY.md` is the index (one line per memory) loaded into context each session.
- `[[wiki-links]]` cross-link memories (and become graph edges).
- `save_memory.sh` / `delete_memory.sh` are the only writers; they hold an flock on the
  index and (if `CLAUDE_MEMORY_REPO` is set) mirror to GitHub with sha-conflict retries.

## 2. The pipeline (the memory life-cycle)
```
encode            consolidate            fixate                 recall
transcript ─▶ .staging ─▶ .md store ─▶ (curate/fixate/distill) ─▶ graph ─▶ Claude
 harvest      graduate     (trusted)      light pass + scoring     insert    (MCP)
```
- **Encode — `memory_harvest.py`**: pulls durable candidate facts from the session
  transcript into `.staging/` (quarantined), with provenance (user-direct vs assistant).
- **Graduate — `memory_stage_apply.py`**: gated promotion of clean candidates into recall
  (confidence threshold, dedup, injection check). Ships in dry-run.
- **Consolidate — `/memory-curate`**: clusters narrow siblings into class-level umbrella
  memories (systems consolidation). Human-gated.
- **Fixate — `/memory-fixate` + `memory_score.py`**: deterministic scoring (age, recall
  frequency, distillation-survival, injection-suspicion) graduates a memory
  *suspect → provisional → corroborated → fixed*, and grows its review interval as it stabilizes.
- **Distill — `memory_distill*.py`**: compress a cluster's prose while *mechanically*
  guaranteeing hard-fact coverage (ports/paths/ids preserved verbatim).
- **Promote — `memory_skill_autoinstall.py` / `/memory-to-skill`**: a fixated, procedure-
  rich, user-direct memory can graduate into an installed Claude Code skill (5 safety gates).

Two lenses run over the store: **curation** (structure/dedup) and **fixation**
(trust/security). The light pass runs both cheaply; neither mutates destructively.

## 3. The backend abstraction — `engram_llm.py`
Every LLM/embedding call routes through one module so the same engine runs on any hardware:
- `generate(prompt, role)` → **ollama** (model picked by `tier` preset over the MoE roles)
  or **claude** (`claude -p`, headless).
- `embed(text)` → Ollama `nomic-embed-text` when reachable, else CPU `fastembed`
  (`nomic-embed-text-v1.5`). **Both 768-dim**, so the graph's vectors are stable across
  backends and never need a paid API.
- `memory_ai.py` holds config (`engram.yaml`) and delegates `ollama_generate`/`ollama_embed`
  to `engram_llm`, so existing callers route through it unchanged.

## 4. The graph (associative + temporal index)
- **Neo4j** (loopback, via `graph/docker-compose.yml`) + **Graphiti**.
- `graph_sync.py` is the auto-link orchestrator: new `.md` → LLM extraction (entities +
  edges per `extract_spec.md`) → `memory_graph_insert.py` builds episode/entity/edge nodes
  (each episode stores its verbatim `source_md`, so `memory_graph_export.py` regenerates the
  `.md` byte-exact). `--reconcile` surfaces superseded facts.
- `mg_config.py` wires Graphiti's embedder + reranker through `engram_llm` (so the graph
  works in claude-only mode, no GPU). Authority model v1: **.md-authoritative**, graph is
  the index (graph-authoritative is a documented future mode).
- **How Claude consumes it**: `mg_mcp_server.py` exposes `memory_recall`,
  `memory_search_facts`, `memory_neighbors`, `memory_stats` as MCP tools (`engram-graph`).
  Claude calls them on demand; the `.md` store stays directly readable for verbatim facts.

## 4b. The vector index (Qdrant) — OPTIONAL
A second, independent index over the same `.md` store, for dense semantic search and
fast (ANN) dedup. **Off by default**; enable with `./install.sh --vector`. With it
disabled or Qdrant unreachable, engram falls back to pure markdown (the in-memory
cosine duplicate finder + graph recall) — nothing breaks.
- **Qdrant** (loopback, via `vector/docker-compose.yml`), driven natively by
  `qdrant-client`. Embeddings route through `engram_llm.embed()` (768-dim), the *same*
  space as the graph — so there is one embedding stack, not two.
- `vector_store.py` (`EngramVectorStore`) maps one `.md` → one point with a deterministic
  uuid5 id (re-insert upserts, never duplicates); payload carries `{file, name,
  description, type, slug, sha}` for filtered search/delete and staleness checks.
- `vector_sync.py` is the sha-synced orchestrator (`--insert`/`--rebuild`/`--delete`/
  `--status`), mirroring `graph_sync.py`. `save_memory.sh`/`delete_memory.sh` fire
  best-effort, non-blocking single-file syncs; the daemon does the bulk cadence.
- The light pass (`memory_light_curate.py`) uses Qdrant ANN for the duplicate finder
  when enabled (O(n·log n)), falling back to the O(n²) cosine loop otherwise.
- Authority model: **.md-authoritative**. Qdrant is a *rebuildable* index — losing it
  never loses a memory; `--rebuild` regenerates it. `ensure_collection` guards against an
  embedding-dim mismatch (model swap → rebuild).
- **How Claude consumes it**: `vector_mcp_server.py` exposes `memory_vector_search` and
  `memory_vector_stats` (`engram-vector`) for raw inspection; recall itself is served by
  `engram-rust`. Vector search takes a `type` filter and scopes to the current store `slug`
  by default (`recall.scope_to_slug`).

## 4c. Hybrid recall (Reciprocal Rank Fusion)
engram has three recall paths — **graph** (associative/temporal), **vector** (dense
semantic), and **keyword** (BM25 lexical, `bin/memory_keyword.py` — pure-python over the
`.md` store, catches exact ids/ports/paths embeddings blur). `memory_recall_hybrid` fuses
them into one ranking with **RRF** (`bin/memory_fusion.py`): each memory scores
`Σ weight/(k_rrf + rank)` across the rankers that returned it, keyed by the `.md` filename
(the shared join key: Qdrant payload `file`, graph `e.file`, keyword over filenames).
- **Where it runs**: the full 3-way tool (`memory_recall` / `memory_recall_hybrid`) lives on
  the **engram-rust** (`engram-mcp`) server, which embeds the query **once** and degrades to
  keyword+vector on no-graph installs. It is the single recall path. `engram-graph` and
  `engram-vector` deliberately no longer expose recall: each ran its own graph/vector legs
  and re-embedded the query, so a recall driven through one of them plus engram-rust called
  the embedding model twice for the same text. They keep only their unique tools
  (graph: facts/neighbours/stats; vector: raw search/stats). The Python `bin/memory_recall.py`
  (TUI/CLI) also embeds once now — when its graphiti graph leg runs, the redundant Qdrant
  leg is skipped, mirroring the Rust compat path.
- **Graceful degradation**: each ranker is independently try/excepted — a disabled/down
  vector store or graph simply drops out of the fusion; keyword is effectively always there.
- **Config**: `recall.hybrid` (`k_rrf`, `default_k`, per-ranker `weights`) +
  `recall.scope_to_slug` in `engram.yaml`.

## 4d. The wiki corpus (Obsidian) — OPTIONAL

A **third** index, over a different shape of document. Memories are atomic facts,
one file one vector. A wiki page is the opposite: long, sectioned, and useless as a
single vector — the memory indexer truncates a body at 1,500 bytes, so the rest of
a 4,000-word runbook is simply not indexed.

- One Obsidian vault per **tenant** (`tenants.<name>.vault`), indexed read-only
  except for one agent-writable subtree (`_agent/` by default).
- `engram-wiki` walks, parses and chunks; `engram-wiki-index` embeds and stores.
  Chunking is heading-aware and every chunk carries its breadcrumb
  (`Page > Section > Subsection`) into the embedding, because a chunk is retrieved
  alone and needs to say what it is about.
- Its own Qdrant collection per tenant (`engram_wiki__<tenant>`), never shared with
  memories: a 40-chunk page would otherwise outvote every memory in one ranking,
  and the prompt hook injects from memory recall on every prompt.
- Authority model: **vault-authoritative and rebuildable**, like Qdrant for
  memories. Losing the index loses nothing; `--rebuild` regenerates it. Freshness
  folds in the embedding space and the chunker version, so a model swap or a
  boundary change invalidates stored chunks instead of leaving them stale.
- **How Claude consumes it**: `wiki_search` finds sections, `wiki_fetch` returns
  the surrounding document within a budget, `wiki_write` files a note into the
  agent subtree (redacted before the bytes reach disk). Memory recall is
  untouched — wiki is retrieved on demand, never injected per prompt.

## 4d-bis. The editor layer (SilverBullet) — OPTIONAL

The vault (§4d) is plain markdown on a headless server, which raises "how does a
human edit it remotely". **SilverBullet** answers it without a sync layer: it is a
server-first editor that serves a server-side directory over the browser, so it
edits the SAME files engram indexes. One instance per tenant (isolation at the
container-mount layer — each sees only its own vault), loopback-bound behind the
Traefik gateway over the tailnet, work vaults read-only by default. engram stays
the indexer and the agents' writer; SilverBullet is the human's editor. The only
file it writes into a space is `.silverbullet.auth.json`, a dotfile the walker
already ignores. See `silverbullet/README.md`.

## 4e. Tenants (agent identities)

A **tenant** is one agent's world: the memory stores it owns and the vault it
reads. The rule it enforces is absolute — a work agent must never recall a homelab
memory or wiki page.

- Qdrant: a collection per tenant, named from the resolved tenant, so reaching
  another identity's vectors requires *naming* its collection rather than
  forgetting a filter.
- Neo4j: a `tenant` property on engram nodes and `group_id` on Graphiti's. Neo4j
  Community has one database, so a unit test scans every Cypher statement in
  `engram-graph` and fails the build if any node pattern is unscoped.
- The markdown store is read off disk, where no database filter would help, so a
  slug belonging to another identity is refused rather than filtered.
- **The identity follows the store.** The slug→tenant mapping is operator-declared
  and total, so naming a store determines the identity. `--tenant` is needed only
  where no store can be determined (the API service), and an unclaimed store is
  refused rather than adopted.

## 5. The daemon (24/7) — `daemon/engram-daemon.py`
A supervisor that runs tasks on independent cadences (a due-check against
`daemon_state.json` means one timer yields per-task intervals):
`health` (backend + Neo4j + Qdrant when enabled) · `graph` (`graph_sync --insert`) ·
`vector` (`vector_sync --insert`, when enabled) · `maintenance` (light pass + pipeline,
gated) · `export --verify` · `reconcile`. Two shapes: **systemd** user timer (`--once`)
or the **claude-only loop container** (`--loop`, `daemon/docker-compose.yml`).
Respects `local_enabled` and the dry-run gates.

## 6. File map
```
bin/      engine + engram_llm.py + memory_recall.py + engram-tui.py → ~/.claude/
bin/hooks/ memory-recall-inject.py (UserPromptSubmit auto-recall)  → ~/.claude/hooks/
commands/ the 6 /memory-* slash commands                        → ~/.claude/commands/
graph/    Graphiti/Neo4j: graph_sync, mg_config, mcp_server, …  → ~/.claude/graph/ (+ venv)
vector/   OPTIONAL Qdrant: vector_store, vector_sync, mcp_server → ~/.claude/vector/ (+ venv)
daemon/   engram-daemon.py, systemd units, Dockerfile + compose
examples/memory/  synthetic seed memories (the AcmeCorp stack)
```
