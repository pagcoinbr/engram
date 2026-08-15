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

## What it costs you — measured

Auto-recall runs on **every prompt**, so it has to be cheap in both wall-clock and
context. Numbers below are from a real 358-memory store (Qdrant + Neo4j + Ollama, all
on localhost), `python3` on Linux — reproduce them with the commands underneath.

| | time |
|---|---|
| **Auto-recall hook, end to end** | **~0.27s** |
| ├ Ollama `nomic-embed-text` embed | 0.04s |
| ├ Qdrant vector search | 0.04s |
| ├ BM25 keyword leg (358 memories, pure python) | 0.13s |
| └ Neo4j 1-hop graph facts | 0.07s |
| The same hook using the *client libraries* instead of HTTP | 1.85s |

The gap is import time, not I/O: `import qdrant_client` alone costs **0.78s** to
perform a **0.04s** search, and `mg_config` pulls in graphiti for a 0.07s query. So
the recall path speaks HTTP to Qdrant/Ollama/Neo4j with nothing but the standard
library — which is also why it needs **no venv, no daemon, and no server**.

**Context cost is bounded, not per-prompt.** Auto-recall injects names and one-line
descriptions only — never memory bodies — and never injects the same memory or graph
fact twice in a session:

| session | naive re-injection | engram (deduped) |
|---|---|---|
| 100 prompts, k=4 | ~25,000 tokens | **~500-800 tokens** |

Recall converges: the first prompts about a topic pay for it, the rest are free. A
long session ends up having loaded ~15-30 unique memories in total.

```bash
# time the whole hook against your own store
echo '{"prompt":"how does X work","session_id":"bench"}' > /tmp/prompt.json
time ~/.claude/hooks/memory-recall-inject.py < /tmp/prompt.json

# why was it quiet? (gated / not importable / disabled / already injected)
ENGRAM_HOOK_DEBUG=1 ~/.claude/hooks/memory-recall-inject.py < /tmp/prompt.json

# the recall core alone — add --json for the raw fused ranking
time ~/.claude/memory_recall.py "how does X work" --k 4 --fast
```

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

## Updating an existing install

`install.sh` is **idempotent — re-running it is the updater.** It refreshes the code
in `~/.claude`, **preserves your `engram.yaml` and your `daemon.env` secrets** (the ccg
key, the Telegram token), and re-registers the MCP servers without duplicating them.

```bash
cd engram && git pull            # get the new code
./install.sh                     # re-run: refreshes ~/.claude, keeps your config + secrets
systemctl --user restart engram.timer   # (systemd daemon) pick up the new code
# then restart Claude Code so it reloads the commands + MCP servers
```

Two things to do by hand after an update:

1. **New config keys.** Because your `engram.yaml` is preserved, keys added since you
   installed are *not* injected — they fall back to safe code defaults, but you won't
   get new features/cadences until you opt in. The installer prints which top-level keys
   are new; diff `engram.yaml.example` against your `~/.claude/engram.yaml` and add the
   blocks you want (e.g. `auto_curate`, `telegram`, `review_gate`, `harvest.idle_minutes`,
   `daemon.intervals`). See **AUTONOMY.md** for what each does.
2. **Vector re-index (only if crossing the body-embedding change).** Older installs
   embedded titles only; run once so recall uses full-body vectors:
   `~/.claude/vector/venv/bin/python ~/.claude/vector/vector_sync.py --rebuild`.

Nothing destructive turns on from an update: the risky autonomy flags
(`auto_graduate`, `auto_curate`, `skill_autoinstall`) stay at whatever you had, and
ship **off** by default.

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
- **Auto-recall** (the `UserPromptSubmit` hook): every prompt gets the memories that match it injected automatically — no waiting for Claude to think of calling a recall tool. Names + one-line descriptions only, **at most once per memory per session**, ~0.3s, fail-open. Off with `recall.inject.enabled: false`.
- **Graph recall** (`engram-graph` MCP): `memory_recall`, `memory_search_facts`, `memory_neighbors`, `memory_stats` — Claude loads only the relevant memories on demand, instead of dumping the whole store into context.
- **Hybrid recall** (`memory_recall_hybrid`, on `engram-graph`): the best single recall — fuses graph + vector + keyword (BM25) into one ranking via Reciprocal Rank Fusion, keyed by the memory filename. Each ranker degrades independently; optional `type` filter.
- **Vector recall** (the optional `engram-vector` MCP): `memory_vector_recall`, `memory_vector_search`, `memory_vector_stats` — dense semantic search via Qdrant. Plus `memory_recall_fused` (vector+keyword) for no-graph installs. Off by default; enable with `./install.sh --vector`.

- **Local-LLM recall** (`hermes`): if the `hermes` CLI is on `PATH`, the installer also registers the same MCP servers with it, so a **local Ollama model** can recall your memories from the terminal — `hermes -z "recall what you know about X"`. Auto-detected; skip it with `./install.sh --no-hermes`. (Plain `ollama run`/`ollama agent` can't do this — ollama has no MCP client.)

### Automatic
A Stop hook harvests new facts each session; the daemon consolidates / fixates / syncs the graph (and the vector index, if enabled) on a cadence — all dry-run + human-approved.

### The console
`~/.claude/engram-tui.py` — a terminal UI over the whole store: dashboard (backend /
graph / vector health), memories (browse, search, view, edit, save, delete), recall,
vector search + re-sync, graph entity lookup, skills, and the staging/quarantine
queues. Pure stdlib `curses`, no server and no browser; saves and deletes go through
`save_memory.sh` / `delete_memory.sh`, the same gates the CLI uses.

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
