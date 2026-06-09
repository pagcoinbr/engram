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

## 5. The daemon (24/7) — `daemon/engram-daemon.py`
A supervisor that runs tasks on independent cadences (a due-check against
`daemon_state.json` means one timer yields per-task intervals):
`health` (backend + Neo4j) · `graph` (`graph_sync --insert`) · `maintenance` (light pass +
pipeline, gated) · `export --verify` · `reconcile`. Two shapes: **systemd** user timer
(`--once`) or the **claude-only loop container** (`--loop`, `daemon/docker-compose.yml`).
Respects `local_enabled` and the dry-run gates.

## 6. File map
```
bin/      engine + engram_llm.py (+ engram_api.py for the GUI)  → installs to ~/.claude/
commands/ the 6 /memory-* slash commands                        → ~/.claude/commands/
graph/    Graphiti/Neo4j: graph_sync, mg_config, mcp_server, …  → ~/.claude/graph/ (+ venv)
daemon/   engram-daemon.py, systemd units, Dockerfile + compose
ui/       React/Vite GUI (FastAPI backend = bin/engram_api.py)
examples/memory/  synthetic seed memories (the AcmeCorp stack)
```
