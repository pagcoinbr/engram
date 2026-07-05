<p align="center">
  <img src="assets/engram-logo.svg" alt="engram — neuro-inspired memory for Claude Code" width="840">
</p>

<p align="center"><b>A neuro-inspired memory organism for Claude Code.</b></p>

An *engram* is the physical trace a memory leaves in the brain. This is that, for your
AI: a local-first memory system that *encodes* what you do, *consolidates* it into
durable knowledge, *fixates* what proves true, and *recalls* it associatively — so
Claude Code remembers your projects, your infra, and your preferences across sessions.

It's not a flat notes file. It's a pipeline modeled on how human memory actually works:

| Human memory | engram |
|---|---|
| **Encoding** (sensory → short-term) | `harvest` — pull durable candidate facts from session transcripts |
| **Working memory** | `.staging/` — quarantined candidates, not yet trusted |
| **Systems consolidation** (hippocampus → neocortex) | `/memory-curate` — cluster narrow facts into class-level "umbrella" memories |
| **Long-term potentiation / reconsolidation** | `/memory-fixate` — memories graduate *suspect → provisional → corroborated → fixed* |
| **The forgetting curve / schema abstraction** | `distill` — compress clusters, drop noise (verbatim hard-facts preserved) |
| **Associative recall** | a **Neo4j knowledge graph** (Graphiti) + embeddings — semantic + multi-hop + temporal |
| **Immune system** | an injection guard that quarantines suspicious/poisoned memories |

Your memories are plain Markdown files (`.md`) — the source of truth, readable and
git-friendly. The graph is a continuously-synced *index* over them.

## How it works — the lifecycle of a memory

engram is a pipeline, not a notes file. A fact moves through stages; you (or the
daemon) drive it forward, and it earns trust as it goes:

1. **Encode** — a Stop hook *harvests* durable candidate facts from the session
   transcript into `.staging/` (quarantined, not yet trusted).
2. **Graduate** — candidates that pass provenance + dedup + injection checks become
   real `.md` memories. Off by default: the pipeline prepares, a human approves.
3. **Consolidate** — `/memory-curate` clusters narrow, overlapping facts and merges
   them into class-level "umbrella" memories; stale ones are pruned.
4. **Fixate** — `/memory-fixate` scores each memory (age + how often it recurs + how
   many distillations it survived + injection-suspicion) and graduates it
   *suspect → provisional → corroborated → fixed*. Trusted memories get distilled
   tighter and reviewed less often; suspect (possibly-poisoned) ones are gated through you.
5. **Recall** — the `.md` store is continuously synced into a **Neo4j graph** and a
   **Qdrant vector index**. At query time engram fuses graph (associative/temporal) +
   vector (semantic) + keyword (BM25) via Reciprocal Rank Fusion, so Claude loads only
   the memories relevant to the task instead of the whole store.

The `.md` files are always the source of truth; the graph and vector index are
**rebuildable indexes** over them. Everything runs locally, and every automated
mutation is **dry-run + human-approved**.

---

## Why you'd want it
- **Persistent project memory** — Claude recalls your architecture, gotchas, and decisions instead of re-learning them every session.
- **Associative recall** — ask "what depends on `db-1`?" and the graph traverses across many memories; semantic search finds the right memory even with no keyword overlap.
- **Runs on your terms** — local Ollama (GPU) *or* Claude-only (no GPU). Your data stays on your machine unless you opt into sync.
- **Safe by default** — every automated mutation is dry-run + human-approved.

## Backends — pick what your hardware allows
- **`ollama`** — local models on a GPU box, free + private. Choose a `tier` for your VRAM (`cpu`/`small`/`medium`/`large`).
- **`claude`** — no GPU: an always-on loop container runs the pipeline via the `claude` CLI. Cost = Claude usage instead of a GPU.

Either way, **embeddings are always local** (Ollama `nomic-embed-text`, or CPU `fastembed`) — the graph never needs a paid embeddings API.

---

## Quickstart

```bash
git clone https://github.com/pagcoinbr/engram.git
cd engram

# interactive — it asks: backend, tier, storage, daemon, graph
./install.sh

# or non-interactive, e.g. a no-GPU setup with the always-on container:
./install.sh --backend claude --storage local --daemon docker --graph --yes
```

The installer copies the engine into `~/.claude`, writes `engram.yaml`, registers the
`/memory-*` commands + the graph recall MCP server, and (optionally) sets up the 24/7
daemon. Restart Claude Code afterward so it loads the new commands.

> **Trying it without touching an existing `~/.claude`?** Install into a sandbox:
> `ENGRAM_CLAUDE_HOME=~/engram-sandbox/.claude ./install.sh --yes`

## How Claude uses it

### Commands
Run in any Claude session. Each is **dry-run first** — it shows a plan and you approve before anything is written or deleted.

| Command | What it does |
|---|---|
| **`/memory-checkpoint`** | Review the current session and save any new, durable facts as memories (dedup-aware). |
| **`/memory-curate`** | *Systems consolidation.* Cluster narrow / overlapping facts and merge them into class-level "umbrella" memories; prune the stale ones. |
| **`/memory-fixate`** | *Long-term potentiation.* Score memories (age + recurrence + distillation-survival + injection-suspicion), distill/merge the trusted ones, and gate suspect (possibly-poisoned) memories through you. |
| **`/memory-reformat`** | Rewrite memories into the canonical **Summary → Index → Body** shape so each file is graspable at a glance (facts preserved, nothing dropped). |
| **`/memory-clean-review`** | Walk the store file-by-file with you deciding **keep / edit / delete** on each — fully human-driven. |
| **`/memory-to-skill`** | Promote a high-trust, frequently-recalled *procedural* memory into a first-class Claude Code skill. |

### Recall — how the right memories reach Claude
- **Graph recall** (`engram-graph` MCP): `memory_recall`, `memory_search_facts`, `memory_neighbors`, `memory_stats` — Claude loads only the relevant memories on demand, instead of dumping the whole store into context.
- **Hybrid recall** (`memory_recall_hybrid`, on `engram-graph`): the best single recall — fuses graph + vector + keyword (BM25) into one ranking via Reciprocal Rank Fusion, keyed by the memory filename. Each ranker degrades independently; optional `type` filter.
- **Vector recall** (the optional `engram-vector` MCP): `memory_vector_recall`, `memory_vector_search`, `memory_vector_stats` — dense semantic search via Qdrant. Plus `memory_recall_fused` (vector+keyword) for no-graph installs. Off by default; enable with `./install.sh --vector`.

### Automatic
A Stop hook harvests new facts each session; the daemon consolidates / fixates / syncs the graph (and the vector index, if enabled) on a cadence — all dry-run + human-approved.

## The 24/7 daemon
`engram-daemon` runs the pipeline + graph sync + health on independent cadences:
- **systemd** (GPU/host): a user timer fires `engram-daemon --once`.
- **container** (claude-only): `docker compose up` in `daemon/` runs Neo4j + the loop.

## Safety & privacy
- **Local-first.** No account needed; memories live in `~/.claude/projects/<slug>/memory`. GitHub sync is opt-in (`--storage github`).
- **Dry-run by default.** Auto-graduate and skill-auto-install ship **off** — the pipeline prepares, a human approves mutations.
- **Injection guard.** Suspicious memories are quarantined out of recall (reversibly).
- **Embeddings never leave the machine.**

## Architecture
```
 transcript ──harvest──▶ .staging ──graduate──▶  .md store  ◀──curate/fixate──┐
 (Claude session)        (quarantine)            (SOURCE OF TRUTH, local)     │
        │                                            │  ▲                     │
        │                                     insert │  │ export (byte-exact) │
        ▼                                            ▼  │                     │
   recall (MCP) ◀──────── memory_recall ◀─── Neo4j graph (Graphiti) ──────────┘
        ▲                                     associative / temporal
        └──── memory_vector_recall ◀─── Qdrant vector index (OPTIONAL) ───────┘
                                        dense semantic search + fast dedup
        all LLM + embedding calls route through engram_llm:
           ollama (tiered)  |  claude (claude -p)   ·   embed: ollama nomic | fastembed (CPU)
        supervised 24/7 by engram-daemon (systemd timer OR docker compose)
```
> Both the graph and the vector index are **optional, rebuildable indexes** over the
> `.md` store. With neither (or with their services down), engram still runs on pure
> markdown. Add the vector index with `./install.sh --vector` (see [vector/README.md](vector/README.md)).

See **[ARCHITECTURE.md](ARCHITECTURE.md)** for the full data flow and **[CONFIG.md](CONFIG.md)** for `engram.yaml`.

## Requirements
- `python3`, `jq` (engine). `git`/`gh` for optional sync.
- Graph: Docker (Neo4j) + a Python venv (graphiti-core, neo4j, fastembed) — the installer builds it.
- Vector index (optional): Docker (Qdrant) + a Python venv (qdrant-client, mcp, fastembed) — `./install.sh --vector` builds it.
- A backend: a reachable Ollama, **or** the `claude` CLI + an Anthropic API key.

## Status
Early but functional: the engine, commands, installer, graph wiring, and daemon are
built and tested. A fresh install is also the first live exercise of the graph
round-trip against your Neo4j/backend. Issues and PRs welcome.

## License
Apache-2.0.
