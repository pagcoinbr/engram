# engram + vector DB (Qdrant) — implementation plan

**Branch:** `feat/qdrant-vector-index`
**Goal:** Add an **optional** Qdrant vector store as a synced semantic index over the
markdown store, replacing the O(n²) in-memory cosine duplicate finder and augmenting
recall — **without** displacing markdown as the source of truth.

---

## 1. Decisions (locked)

| Decision | Choice | Consequence |
|---|---|---|
| Source of truth | **Markdown `.md` stays canonical** | Qdrant is a derived, re-buildable index. Git sync, byte-exact graph export, and `MEMORY.md` are untouched. |
| Vector DB | **Qdrant** (matches the mem0 reference) | New optional service (Docker), parallel to the existing optional Neo4j. |
| Relationship to Neo4j graph | **Keep the graph; add Qdrant alongside** | Qdrant = dense semantic search + dedup; Neo4j/Graphiti = relational, multi-hop, temporal. Both index the `.md` store. |
| Integration style | **Native** — `qdrant-client` fed by `engram_llm.embed()` | No `mem0ai` stack. Reuses engram's 768-dim embedding abstraction (Ollama `nomic` → `fastembed` fallback). One embedding space across graph + vector DB. |
| **Optionality** | **Off by default; pure-markdown fallback** | Like `--graph`. With Qdrant disabled/unreachable, engram behaves exactly as today (cosine duplicate finder, graph recall). No hard dependency. |

### Why native (not the `mem0ai` library)
mem0's reference uses `Memory.from_config(...)` which drags in its own embedder, LLM,
and Neo4j-graph stack. engram already owns all three (`engram_llm.py`, tiered Ollama,
Graphiti). Adopting `mem0ai` would create a **second, conflicting** embedding/graph
pipeline. We instead borrow mem0's *patterns* (config-driven Qdrant client, collection
with `embedding_model_dims`, metadata-scoped points, payload filters, safe per-point
delete, lazy init) and drive them with `engram_llm.embed()`.

### What we keep from mem0 (patterns, not code)
- Config dict shape: `{collection_name, url, embedding_model_dims, api_key?, on_disk?}`.
- Collection auto-create with explicit vector size + cosine distance.
- Per-point payload carrying scope/metadata; filtered search/list/delete.
- **Never** a blanket `reset()`/`delete_all()` on the write path — iterate + delete by id.
- Lazy client init so the engine runs fine when Qdrant is down.
- `on_disk` flag to bound RAM.

---

## 2. Current state (what we build on)

- **Store:** `~/.claude/projects/<slug>/memory/*.md`, written only by `bin/save_memory.sh`,
  `bin/save_memory_content_only.sh`, `bin/delete_memory.sh` (via `bin/memory_lib.sh`,
  flock on `MEMORY.md`). Frontmatter `name`/`description`/`type`.
- **Embeddings:** `engram_llm.embed(text)` → **768-dim** (`nomic-embed-text` via Ollama, else
  `fastembed nomic-embed-text-v1.5`). `engram_llm.embed_dim()` returns the dim.
  `memory_ai.ollama_embed()` is the thin wrapper callers use.
- **Similarity today (the thing we replace):** `bin/memory_light_curate.py` embeds every
  file's `name+description` and does an **O(n²) pairwise cosine** to flag near-dupes
  (`light_pass.duplicate_finder.dup_threshold`, default 0.86). Report-only.
- **Graph (optional, parallel precedent):** `graph/` — `graph_sync.py` orchestrates
  extract→insert; `mg_config.py` wraps `engram_llm.embed` as Graphiti's embedder;
  `mg_mcp_server.py` exposes `memory_recall`/`memory_search_facts`/`memory_neighbors`/
  `memory_stats` as the `engram-graph` MCP server. `insert_state.json`/sync map tracks
  which `.md` are indexed (sha-based change detection). Neo4j runs via
  `graph/docker-compose.yml`; venv + MCP wired by `install.sh` behind `--graph`.
- **Config:** `engram.yaml` → `memory_ai.load()` (deep-merge over `_DEFAULTS`).
- **Installer:** `install.sh` builds an optional venv and registers MCP behind a flag.

This is the **template**: Qdrant integration mirrors the graph's optional, config-gated,
sha-synced, MCP-exposed shape.

---

## 3. Architecture after this change

```
                         ┌────────────── .md store (SOURCE OF TRUTH, local) ───────────────┐
 transcript ─harvest─▶ .staging ─graduate─▶  *.md  +  MEMORY.md  ◀── curate / fixate ───────┤
                                              │  │                                          │
                          save/delete hooks ──┤  ├── graph_sync  ──▶ Neo4j (Graphiti)  ─────┤  relational/temporal
                                              │  └── vector_sync ──▶ Qdrant (NEW, optional)─┘  dense semantic
                                              ▼
                                    recall (MCP):  engram-graph  +  engram-vector (NEW)
        embeddings for BOTH indexes route through engram_llm.embed()  (768-dim, local)
        Qdrant disabled/unreachable  ⇒  fall back to pure-markdown (cosine dupe finder, graph-only recall)
```

Qdrant is a **rebuildable index**: `vector_sync.py --rebuild` re-embeds the whole `.md`
store from scratch. Losing Qdrant never loses a memory.

---

## 4. New files

```
vector/                         # mirrors graph/ layout
  vector_store.py               # EngramVectorStore: thin qdrant-client wrapper (ensure/upsert/search/delete/list/stats)
  vector_sync.py                # CLI orchestrator: --insert / --rebuild / --status / --delete (sha-synced like graph_sync)
  vector_mcp_server.py          # FastMCP "engram-vector": memory_vector_recall / memory_vector_search / memory_vector_stats
  vector_config.py             # reads engram.yaml vector_store.* + env; builds QdrantClient; small helpers
  docker-compose.yml            # qdrant/qdrant:latest on 127.0.0.1:6333 (loopback only), named volume
  README.md                     # ops notes (start/stop, rebuild, disable)
```

No new heavyweight venv unless needed: `qdrant-client` is pure-Python and can install
into the **existing graph venv** when `--graph` is on, or a dedicated `vector/venv`
when vector-only. (See §8.)

---

## 5. Module designs

### 5.1 `vector/vector_config.py`
- Resolve config from `memory_ai.load()['vector_store']` (see §6) + env overrides
  (`ENGRAM_QDRANT_URL`, `ENGRAM_QDRANT_API_KEY`, `ENGRAM_VECTOR_COLLECTION`).
- `enabled()` → bool (master gate; default **False**).
- `build_client()` → `QdrantClient(url=..., api_key=..., timeout=...)`, **lazy**; raises a
  typed `VectorUnavailable` on connection failure so callers fall back cleanly.
- `collection_name()`, `dim()` (= `engram_llm.embed_dim()`, 768), `distance=COSINE`.

### 5.2 `vector/vector_store.py` — `class EngramVectorStore`
Thin, dependency-light wrapper (patterns lifted from mem0's Qdrant usage, embeddings from
`engram_llm`):
- `ensure_collection()` — create if missing with `size=dim`, `distance=Cosine`,
  `on_disk` per config. Idempotent; verifies existing dim matches (warns + offers rebuild
  on mismatch — e.g. if someone swaps embedding models).
- `point_id(filename)` — **deterministic UUIDv5** from the filename so re-inserting the
  same `.md` upserts (no dupes), and delete-by-name is O(1).
- `upsert(filename, name, description, type, body_excerpt, vector)` — payload carries
  `{file, name, description, type, slug, sha}` for filtered search/list/delete and
  staleness checks. `slug` scopes multi-store setups (parallels graph `group_id`).
- `search(query, k, threshold)` — embed query via `engram_llm.embed`, `query_points`,
  return `[{file, name, description, score}]`.
- `find_duplicates(threshold)` — for each point, search its own vector for nearest others
  ≥ threshold; returns pairs. **Replaces the O(n²) loop** (now O(n·log n) ANN).
- `delete(filename)`, `list(slug=None)`, `stats()` (count via `count()`).
- Every method tolerates `VectorUnavailable` by raising it up; **no method ever calls a
  collection-wide reset on the write path** (mem0 lesson).

### 5.3 `vector/vector_sync.py` — orchestrator (mirrors `graph_sync.py`)
- Sha-synced like the graph: `vector/sync_state.json` maps `filename → sha` so only
  new/changed `.md` get re-embedded.
- `--insert` — embed+upsert new/changed files (drops removed files from Qdrant).
- `--rebuild` — drop + recreate collection, re-embed every `.md`.
- `--status` — store count vs indexed count vs pending (like `graph_sync --status`).
- `--delete <file>` — remove one point.
- Honors `local_enabled` and `vector_store.enabled`; **no-op with a clear message when
  disabled or unreachable** (never errors the caller).

### 5.4 `vector/vector_mcp_server.py` — `engram-vector` MCP
FastMCP server parallel to `mg_mcp_server.py`:
- `memory_vector_recall(query, k=6)` — semantic top-k: name + description + score. The
  fast path for "load the relevant memories" when the graph is off or for pure-vector recall.
- `memory_vector_search(query, k=8)` — raw scored hits.
- `memory_vector_stats()` — point count, collection, dim, on_disk.
- Returns a friendly "(vector store disabled/unreachable — using markdown)" string instead
  of throwing when Qdrant is down, so Claude degrades gracefully.

---

## 6. Config additions (`engram.yaml` + `engram.yaml.example` + `memory_ai.py _DEFAULTS`)

```yaml
# ── Vector store (OPTIONAL) ─────────────────────────────────────────────────
# A Qdrant index over the .md store for semantic recall + fast dedup. OFF by
# default: with this disabled or Qdrant unreachable, engram falls back to pure
# markdown (the in-memory cosine duplicate finder + graph recall) — nothing breaks.
vector_store:
  enabled: false                 # master switch (parallels the optional graph)
  provider: qdrant               # qdrant (only backend for now)
  url: "http://127.0.0.1:6333"   # loopback Qdrant
  api_key: ""                    # for Qdrant Cloud; blank for local
  collection: "engram_memory"
  on_disk: false                 # true = store vectors on disk (less RAM, slower)
  timeout_seconds: 30
  dim: 768                       # MUST match embed.dim (nomic). Auto-derived; here for clarity.
  recall:
    default_k: 6
    threshold: 0.0               # min cosine for search hits (0 = no floor)
  duplicate_finder:
    use_vector_store: true       # when enabled, the light pass uses Qdrant ANN instead of O(n²) cosine
```

- `memory_ai._DEFAULTS` gains the `vector_store` block (default disabled) so missing keys
  resolve safely.
- Add `memory_ai.vector_enabled(cfg)` helper, alongside `local_enabled`.

---

## 7. Wiring into existing flows (all behind `vector_store.enabled`)

1. **Light pass / duplicate finder — `bin/memory_light_curate.py`** (primary win)
   - If `vector_store.enabled` **and** `duplicate_finder.use_vector_store`: call
     `EngramVectorStore.find_duplicates(thr)` (ANN) instead of the O(n²) cosine block.
   - Else: **unchanged** current cosine path (the markdown fallback).
   - Output format identical, so `/memory-curate` consumers don't change.

2. **Save path — `bin/save_memory.sh` / `save_memory_content_only.sh`**
   - After a successful local write, fire a **non-blocking, best-effort** index nudge:
     `vector_sync.py --insert --only <file>` (guarded by `vector_store.enabled`; failure
     is logged, never fatal — same posture as the optional GitHub push).
   - Keep it cheap: a single embed+upsert. (Bulk/scheduled sync still covered by the daemon.)

3. **Delete path — `bin/delete_memory.sh`**
   - After removing the `.md`, call `vector_sync.py --delete <file>` (guarded, best-effort).

4. **Daemon — `daemon/engram-daemon.py`**
   - Add a `vector` task (cadence next to `graph`, e.g. `intervals.vector: 1800`) running
     `vector_sync.py --insert`. Add a Qdrant reachability line to the `health` task.
   - Respects `local_enabled` + `vector_store.enabled`; no-op when off.

5. **Recall**
   - New `engram-vector` MCP server registered alongside `engram-graph`. Graph recall
     stays the default associative path; vector recall is the dense-semantic fast path and
     the **only** recall path in vector-only (no-graph) installs.

No changes to `memory_lib.sh` locking, `MEMORY.md`, or the graph's byte-exact export.

---

## 8. Installer & ops (`install.sh`, new `--vector` flag)

- New flag `--vector | --no-vector` (and an interactive prompt), default **no** — exactly
  like `--graph`.
- When `--vector`:
  - Install `qdrant-client` into the graph venv (if `--graph`) or a dedicated `vector/venv`.
  - Copy `vector/*.py` + `vector/docker-compose.yml` to `~/.claude/vector/`.
  - Register MCP: `claude mcp add --scope user engram-vector <venv>/bin/python ~/.claude/vector/vector_mcp_server.py`.
  - Flip `vector_store.enabled: true` in the written `engram.yaml`.
  - Print the start command: `cd ~/.claude/vector && docker compose up -d`.
- `uninstall.sh`: deregister `engram-vector`, optional volume teardown note.
- `vector/docker-compose.yml`: `qdrant/qdrant`, ports bound to `127.0.0.1:6333` only,
  named volume `engram_qdrant`.

---

## 9. Dependencies

- Add `qdrant-client` (pure-Python; pulls `grpcio`/`httpx`) to the relevant venv install
  line(s) in `install.sh`. No change to the core engine's single `pyyaml` dependency —
  the engine still runs with zero vector deps when the feature is off.
- Qdrant server itself: Docker image only (no host install).

---

## 10. Docs

- `README.md`: add Qdrant to the "optional backends" framing; one line in the architecture
  diagram; note "vector store is optional, markdown is the fallback."
- `ARCHITECTURE.md`: a "§4b The vector index (Qdrant)" section parallel to the graph one,
  stating the authority model (`.md`-authoritative, Qdrant is a rebuildable index).
- `CONFIG.md`: document the `vector_store` block.
- `engram.yaml.example`: the annotated block from §6.

---

## 11. Tests / verification

- `vector/smoke_test.py` (parallel to `graph/smoke_test.py`): ensure_collection →
  upsert 3 synthetic memories (reuse `examples/memory/`) → search returns the right top
  hit → find_duplicates flags the intentional near-dupe → delete → stats.
- Fallback test: with `vector_store.enabled: false`, `memory_light_curate.py` still
  produces the cosine report unchanged; save/delete succeed with Qdrant down.
- Dim-mismatch guard test: existing collection at wrong dim → clear warning, no crash.
- `tests/secret-scan.sh` stays green (no secrets; api_key blank by default).

---

## 12. Build order (incremental, each step independently testable)

1. `vector/vector_config.py` + `vector_store.py` + `vector/smoke_test.py` — prove the
   Qdrant round-trip in isolation (Docker up, native embeds).
2. `engram.yaml.example` + `memory_ai._DEFAULTS` + `vector_enabled()` helper.
3. `vector/vector_sync.py` (insert/rebuild/status/delete + sha state).
4. Wire `memory_light_curate.py` (ANN dupes behind the flag; keep cosine fallback).
5. Wire `save_memory.sh` / `delete_memory.sh` best-effort hooks.
6. `vector/vector_mcp_server.py` + register in `install.sh`.
7. `daemon/engram-daemon.py` vector task + health line.
8. `install.sh` / `uninstall.sh` `--vector` plumbing + `vector/docker-compose.yml`.
9. Docs (README / ARCHITECTURE / CONFIG).

Steps 1–4 deliver the core value (fast dedup + a working semantic index) and can land
before the MCP/daemon/installer polish.

---

## 13. Risks & mitigations

| Risk | Mitigation |
|---|---|
| New always-on service (Docker) | Strictly optional + off by default; loopback-bound; pure-markdown fallback. |
| Embedding-model swap → dim/space drift | `ensure_collection` verifies dim; `--rebuild` re-embeds; dim derived from `engram_llm.embed_dim()`. |
| Index drift vs `.md` truth | Sha-synced insert + daemon cadence + `--rebuild`; Qdrant is never authoritative. |
| Write-path latency | Save/delete hooks are best-effort, single-point, non-blocking; bulk work is the daemon's. |
| Duplicate-finder behavior change | Same output format + threshold; flag-gated; cosine path retained as fallback. |
| Two embedding indexes (graph + vector) | Both use the **same** `engram_llm.embed()` (768-dim), so vectors are consistent. |
```
