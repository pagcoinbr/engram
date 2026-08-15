# engram configuration (`engram.yaml`)

The installer writes `~/.claude/engram.yaml` from your answers; edit it any time and
re-run nothing (scripts read it live). Full annotated template: `engram.yaml.example`.
Resolution: env `ENGRAM_CONFIG` > `~/.claude/engram.yaml` > built-in defaults.

## Backend & tier

```yaml
backend: ollama        # ollama | claude
tier: small            # cpu | small | medium | large   (ollama only)
```

**`backend: ollama`** runs local models on a GPU box. The **tier** selects a model
preset over the mixture-of-experts roles (you don't list models unless you want to):

| tier | VRAM | harvest | distill | injection / verify | embeddings |
|---|---|---|---|---|---|
| `cpu` | none | llama3.2:1b | llama3.2:3b | llama3.2:3b | nomic-embed-text |
| `small` | ~8 GB | qwen2.5-coder:7b | llama3.1:8b | llama3.1:8b | nomic-embed-text |
| `medium` | ~16–24 GB | qwen2.5-coder:7b | gpt-oss:20b | deepseek-r1:14b | nomic-embed-text |
| `large` | ≥32 GB | qwen2.5-coder:7b | qwen3-coder:30b | deepseek-r1:32b | nomic-embed-text |

> `cpu` tier is slow — if you have no GPU, prefer `backend: claude`.

**`backend: claude`** needs no GPU; pipeline LLM steps shell out to the `claude` CLI
(`claude -p`, single-turn, no tools). Set `ANTHROPIC_API_KEY` in the daemon container.

```yaml
ollama:
  host: "http://localhost:11434"   # your Ollama endpoint (e.g. a LAN GPU box)
  timeout_seconds: 1200            # raise for big models on slow hardware
claude:
  bin: "claude"
  model: ""                        # blank = CLI default; or pin e.g. "claude-sonnet-4-6"
  max_turns: 1
embed:
  fastembed_model: "nomic-ai/nomic-embed-text-v1.5"   # CPU fallback, 768-dim (matches Ollama nomic)
  dim: 768
```

**Override a single role** regardless of tier:
```yaml
experts:
  distill: { model: "qwen3-coder:30b" }
```

## Storage
- **local** (default): memories in `~/.claude/projects/<slug>/memory`. Nothing leaves the machine.
- **github**: opt-in sync. `./install.sh --storage github --repo owner/name` writes
  `~/.claude/engram.env` with `CLAUDE_MEMORY_REPO`; the engine reads it. Needs `gh` auth.

## Safety gates (ship OFF)
```yaml
auto_graduate:    { enabled: false }   # graduate staged candidates into recall unattended
skill_autoinstall:{ enabled: false }   # auto-install vetted skills (kill-switch: touch ~/.claude/skills/auto/.disabled)
light_pass:       { enabled: true }    # cheap twice-daily pass (scoring/dedup/quarantine) — never mutates destructively
```
Turn the first two on only after you've watched the dry-run output and trust it.

## Daemon cadences
```yaml
daemon:
  intervals: { health: 300, graph: 1800, vector: 1800, maintenance: 21600, export: 86400, reconcile: 86400 }
schedule:
  times: ["03:30", "15:30"]   # systemd timer fallback fire times
```

## Vector store (Qdrant) — optional
A semantic index over the `.md` store for dense recall + fast (ANN) dedup. **Off by
default**; with it disabled or Qdrant unreachable, engram falls back to pure markdown.
Enable with `./install.sh --vector` (which also flips `enabled: true` and registers the
`engram-vector` MCP server). Embeddings reuse `engram_llm.embed()` (768-dim), so the
vector space matches the graph.

```yaml
vector_store:
  enabled: false                 # master switch (parallels the optional --graph)
  provider: qdrant
  url: "http://127.0.0.1:6333"   # loopback Qdrant
  api_key: ""                    # for Qdrant Cloud; blank for local
  collection: "engram_memory"
  on_disk: false                 # true = vectors on disk (less RAM, slower)
  timeout_seconds: 30
  recall:        { default_k: 6, threshold: 0.0 }
  duplicate_finder: { use_vector_store: true }   # light pass uses Qdrant ANN instead of O(n^2) cosine
```
Env overrides: `ENGRAM_QDRANT_URL`, `ENGRAM_QDRANT_API_KEY`, `ENGRAM_VECTOR_COLLECTION`.
The vector venv lives at `~/.claude/vector/venv`. Start the service with
`cd ~/.claude/vector && docker compose up -d`; seed/rebuild with
`vector_sync.py --rebuild`. See [vector/README.md](vector/README.md).

The vector recall/search MCP tools accept a `type` filter
(`user|feedback|project|reference`) and, by default, scope results to the current
store's `slug` (toggle below).

## Hybrid recall (Reciprocal Rank Fusion)
`memory_recall_hybrid` (on `engram-graph`) fuses graph + vector + keyword (BM25) into
one ranking keyed by the memory filename. `memory_recall_fused` (on `engram-vector`)
is the 2-way (vector+keyword) variant for no-graph installs. Each ranker degrades
independently. The installer adds `qdrant-client` to the graph venv on `--vector` so
the warm graph server can query Qdrant in-process.

```yaml
recall:
  scope_to_slug: true            # restrict vector/hybrid recall to the current store's slug
  hybrid:
    enabled: true
    k_rrf: 60                    # RRF constant (standard 60)
    default_k: 6                 # fused memories returned
    weights: { graph: 1.0, vector: 1.0, keyword: 1.0 }
  inject:                        # auto-recall on every prompt (UserPromptSubmit hook)
    enabled: true
    k: 4                         # memories per injection
    max_facts: 6                 # 1-hop graph facts per injection
    timeout_ms: 2500             # per-leg HTTP budget
```

## Auto-recall (the prompt hook)
`hooks/memory-recall-inject.py` runs on every prompt and injects the memories that
match it, so recall does not depend on Claude remembering to call a recall tool. The
installer merges it as a `UserPromptSubmit` hook; `recall.inject.enabled: false`
silences it without unmerging.

It is built to be cheap and quiet:
- **names + one-line descriptions only** — never memory bodies;
- **once per session** — a memory (or graph fact) already injected in this session is
  never injected again, state in `~/.claude/logs/recall-inject/<session_id>.json`,
  swept after 7 days. This is what stops a long session from re-paying for the same
  memories on every prompt;
- **~0.3s, stdlib only** — it talks to Qdrant/Ollama/Neo4j over HTTP rather than
  importing their clients (`import qdrant_client` alone measures 0.78s), so it needs
  no venv and no daemon;
- **fail-open** — any error prints nothing and exits 0. To find out *why* it was
  quiet: `echo '{"prompt":"...","session_id":"dbg"}' | ENGRAM_HOOK_DEBUG=1 ~/.claude/hooks/memory-recall-inject.py`.

A prompt shorter than 25 characters, or starting with `/` or `!`, is skipped.

## Graph (Neo4j)
Config is mostly env: `NEO4J_URI` (default `bolt://127.0.0.1:7687`), `NEO4J_PASSWORD`
(from env or `~/.claude/graph/.env`, generated by the installer). `OLLAMA_BASE_URL`,
`MG_LLM_MODEL` tune the optional bootstrap path. The graph venv lives at
`~/.claude/graph/venv`.
