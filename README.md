# engram

**A neuro-inspired memory organism for Claude Code.**

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
- **Commands** (in any session): `/memory-checkpoint`, `/memory-curate`, `/memory-fixate`, `/memory-to-skill`, `/memory-reformat`, `/memory-clean-review`.
- **Graph recall** (the `engram-graph` MCP server): Claude calls `memory_recall`, `memory_search_facts`, `memory_neighbors`, `memory_stats` on demand to load only the relevant memories — instead of dumping the whole store into context.
- **Hybrid recall** (`memory_recall_hybrid`, on `engram-graph`): the best single recall — fuses graph + vector + keyword (BM25) into one ranking via Reciprocal Rank Fusion, keyed by the memory filename. Each ranker degrades independently; optional `type` filter.
- **Vector recall** (the optional `engram-vector` MCP server): `memory_vector_recall`, `memory_vector_search`, `memory_vector_stats` — dense semantic search over the store via Qdrant (with a `type` filter, scoped to the current store). Plus `memory_recall_fused` (vector+keyword) for no-graph installs. Off by default; enable with `./install.sh --vector`.
- **Automatic**: a Stop hook harvests new facts; the daemon consolidates/fixates/syncs the graph (and the vector index, if enabled) on a cadence.

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
