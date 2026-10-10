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
`memory_recall` / `memory_recall_hybrid` (on `engram-rust`) fuse graph + vector +
keyword (BM25) into one ranking keyed by the memory filename, embedding the query
once; they degrade to vector+keyword on no-graph installs. `engram-graph` and
`engram-vector` no longer expose recall tools (they re-embedded the query, so a
recall through one plus `engram-rust` hit the embedding model twice). Each ranker
degrades independently.

## Rust graph backend

The Rust recall service has two graph backends:

```yaml
graph:
  backend: graphiti_compat       # graphiti_compat | native
```

`graphiti_compat` is the production-safe migration choice, **and the default when
the `graph:` block is absent** — which is the case for almost every install
upgraded in place, since the installer never adds one. It calls the installed,
pinned Graphiti recall path and returns its ordered results untouched.

Note what that means concretely: in compatibility mode the outer **keyword and
vector legs are disabled and there is no RRF**. The whole point is that
compatibility mode cannot change Graphiti's ranking, so it is *compatibility*, not
hybrid fusion — the `legs` map in a recall response says so explicitly
(`"disabled: graphiti_compat preserves Graphiti ordering"`). It fails closed if the
Graphiti path is unavailable, and the child process is bounded by
`recall.timeout_ms`.

`native` uses Rust's typed-triple and embedding index together with the usual
hybrid RRF; use it for shadow evaluation until its recall evaluation reaches
parity. Native node identity includes the memory store slug, so switching to it
requires a graph rebuild, and the legacy-edge cache needs
`engram-native-graph-sync --import-legacy-embeddings` to be re-run.

The native writer needs **both** an OpenAI-compatible embedding endpoint
(`embed.url`, or `llama_cpp.url` as a fallback) and an OpenAI-compatible
generation endpoint for fact extraction (`llama_cpp.url`). Note that this is
`llama_cpp.url` specifically, *not* `backend` — `backend` selects the Python
pipeline's generation backend, so `backend: ollama` with an OpenAI-compatible
`llama_cpp.url` alongside it is a perfectly serviceable native setup.

With `graph.backend: native` and either endpoint missing, the daemon's graph job
**skips and says so** each run rather than falling back to the Python Graphiti
writer: writing the index nothing is reading looks exactly like memories that
saved and then vanished. The config still loads, so recall against an
already-populated native index keeps working. Choose `graphiti_compat` to use the
Python writer.

Endpoint URLs must not embed credentials — `https://user:pass@host/v1` is
rejected. Those URLs are published by `/api/v1/status` and the model editor, which
would route the password around the redaction that `api_key` gets.

`ENGRAM_GRAPH_BACKEND` temporarily overrides this setting for a process, and is
honoured by the **reader and the writer alike** — a daemon that wrote to one index
while recall read the other produced a split brain that looked like missing
memories. The daemon uses the matching writer, export, and reconciliation jobs for
whichever backend is selected, so new memories remain visible to the recall path in
use.

The prompt hook is separate: it always runs in fast mode (local BM25 + vector + one
graph fact query, never the Graphiti child) under the much shorter
`recall.inject.timeout_ms` budget, because a prompt that waits is worse than a
prompt without recall.

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

## Tenants: agent identities

A **tenant** is one agent's world — the memory stores it owns and, optionally, the
Obsidian vault it reads. The point is isolation: a `work` agent must never recall a
`homelab` memory or wiki page.

```yaml
tenants:
  work-company-x:
    slugs: ["-root-MJSV", "-root-dsec-lab"]   # memory stores it owns
    vault: /vaults/work                        # absolute; optional
    agent_subtree: _agent                      # the only agent-writable path
    extract_facts: false                       # LLM facts per wiki section
  homelab:
    slugs: ["-root", "-root-ASCP"]
    vault: /vaults/homelab
```

**An absent block means tenancy is off**, and engram behaves exactly as before: one
shared collection, Graphiti's historical `canonical` group, the slug resolved the
usual way. That is the default and the upgrade path.

### The identity follows the store

**Where a store can be named, the identity is determined.** The slug→tenant mapping
is operator-declared and total, so naming a store has exactly one right answer and
no way to pick wrongly. `engram-index --slug -root-MJSV` writes mjsv's collection
because that *is* mjsv's store.

This is not a convenience. The prompt hook and the MCP server are registered once
in `settings.json`, and the save path fires `engram-index --slug <slug>`
backgrounded with its output discarded — a host with four identities needs all four
served from those fixed call sites. A hard-coded `--tenant` there could serve one,
and requiring the flag did not make anything safer: it made memories silently stop
being indexed.

`--tenant` is therefore needed only where no store can be determined, and acts as a
cross-check when both are given (a slug belonging to another identity is still
refused). Two places genuinely need it:

- **`engram-app`** — one identity per process, with no project directory to derive
  from. A service's working directory, and even its `$HOME`, say nothing about which
  agent it belongs to; the shipped unit sets no `HOME` at all, so the derived store
  there was `-`, the slugification of `/`. `install.sh` writes `ENGRAM_TENANT` into
  `~/.config/engram/daemon.env` when a tenant owns the install's slug.
- **the daemon's children** — it passes `--tenant` and `--slug` explicitly, once per
  store, so one tenant's unreachable backend cannot stop the others.

**A store no tenant claims is refused, never adopted.** That is the half of the rule
that keeps "the identity follows the store" from becoming "the identity is whatever
is nearby". And there is still no default *tenant*: `Tenant::resolve` refuses an
absent one even when exactly one is configured, because "obviously the only one" is
how a default gets established that becomes silently wrong the day a second identity
appears.

### How the boundary is enforced

| Layer | Mechanism |
|---|---|
| Qdrant | A collection per tenant (`engram_memory__<name>`, `engram_wiki__<name>`). The name comes from the resolved tenant, so reaching another identity's vectors means *naming* its collection rather than forgetting a filter. |
| Neo4j | A `tenant` property on every engram node and `group_id` on Graphiti's. Neo4j Community has one database, so a database boundary is not available; instead a unit test scans every Cypher statement in `engram-graph` and fails the build if any node pattern is unscoped. |
| Markdown store | A slug belongs to exactly one tenant. An explicit `--slug` naming another identity's store is refused, not filtered — the store is read straight off disk, where no database filter would help. |
| Vault | Every path is canonicalized and verified to resolve inside the tenant's vault, so a symlink pointing at another vault is refused. Agent writes are confined again, to `agent_subtree`. |
| MCP | `slug` is a tool argument and is validated per call. `tenant` is deliberately **not** a tool argument — it is fixed when the client launches the server, so a model cannot choose its own identity. |

### Migrating existing stores

Moving stores into tenants is not a config-only change. The Qdrant points must be
reindexed into the per-tenant collection, and the Graphiti episodes re-inserted
under the new `group_id` — re-inserted rather than relabelled, because Graphiti
`Entity` nodes are shared across episodes, so an entity mentioned under two
identities is a single node with a single group that no `SET` can split. Cached
extractions in `~/.claude/graph/extractions/` mean the re-insert costs no LLM
calls. A slug claimed by two tenants fails config validation, because that is
exactly the leak the model exists to prevent.

### Known limitation

The legacy keyword leg rides Graphiti's `edge_name_and_fact` fulltext index, and a
Neo4j fulltext index cannot be partitioned: it ranks across every group and applies
its own limit before any group filter can run. The leg over-fetches (20×) and
filters, which bounds the problem without solving it — a tenant holding under
~1/20th of the indexed edges can still be crowded out. The semantic and native
keyword legs do not share the defect, so recall degrades rather than leaking.

## The wiki corpus (Obsidian vaults)

A tenant's `vault:` points at an Obsidian vault, indexed **read-only** apart from
one agent-writable subtree. It is a separate corpus from memories: its own Qdrant
collection, its own payload shape, its own MCP tools.

Separate on purpose. A memory is one atomic fact; a wiki chunk is a fragment of a
long document. Fusing them into one ranking would let a 40-chunk page outvote every
memory in the store — and the prompt hook injects from memory recall on *every*
prompt. Wiki is retrieved when an agent asks for it.

### What gets indexed

Recursive, skipping `.obsidian` (Obsidian's own state), `.trash` (indexing it would
resurrect deleted pages), `.git` and dotfiles. Markdown only.

Chunking is heading-aware: ~400 tokens with ~60 of overlap, splitting long sections
at paragraph boundaries. **Each chunk carries its breadcrumb into the embedding** —
`Runbooks > RabbitMQ > Failover` — because a chunk is retrieved alone and
"restart the broker" is otherwise indistinguishable between four runbooks.

Code fences are respected, so `# comment` inside a shell block is not a heading.
Without that, a page splits at every comment in every snippet and the boundaries
move whenever a sample is edited.

Freshness is per document, keyed on content **plus** the embedding space **plus**
`CHUNKER_VERSION`. Changing chunk parameters therefore invalidates stored chunks
rather than leaving documents looking current.

### Tools

| tool | purpose |
|---|---|
| `wiki_search(q, k)` | matching sections, with breadcrumb, path and score |
| `wiki_fetch(path, heading?, max_chars?)` | whole document or one section, budgeted |
| `wiki_write(path, content, mode?)` | file a note — agent subtree only |

`wiki_fetch` is what makes the feature useful: search finds the section, fetch
returns the surrounding document. A truncated fragment is what the memory path
already gives for a long document.

### Agent writes

Only under `<vault>/<agent_subtree>` (default `_agent/`). Four independent guards:

1. **Containment** — must resolve inside the vault, symlinks and `..` included.
2. **Subtree** — and inside the agent subtree, so curated pages cannot be touched.
   Separate from (1) deliberately: a change to one must not widen the other.
3. **Markdown only** — a sync client propagates whatever is in the vault.
4. **Redaction before the write**, not merely before indexing. A credential in a
   synced vault has left the box whatever the index holds.

Writes are temp-file + fsync + rename, so Obsidian never renders a half-written
page. `mode` is `create` (default, refuses to overwrite), `append`, or `replace`.

### Getting content in

engram only reads. Point the vault directory at Obsidian Sync, Syncthing, git or a
mount — indexing is sha-based, so a file that appears later is picked up on the
next pass with no further configuration. No locking against Obsidian's own file
watcher is needed while agent writes stay inside `_agent/`, which the human does
not edit; that guarantee does **not** extend to widening the subtree.

## Backups (encrypted, off-host)

A daily [restic](https://restic.net) backup of the **authoritative** data — the
`.md` memory stores and the Obsidian vaults, plus `engram.yaml` and `graph/.env`
so a restore stands the system back up. The Qdrant and Neo4j indexes are **not**
backed up: they are rebuildable from the `.md` store (`--rebuild`), so backing
them up would be redundant weight.

Off by default. The daemon's `backup` task no-ops until you enable it and supply
credentials; `./install.sh --backup` provisions everything.

### Why restic

Client-side AES-256 encryption, always on — the data carries infrastructure
detail and the restore set includes `graph/.env`, so the store (Backblaze B2 or
any S3-compatible bucket) must never see plaintext. Also: content-addressed
dedup (a daily snapshot of an unchanged 4 MB set is nearly free), snapshot
history, retention (`forget --prune`), and integrity checking.

### Configuration

The non-secret knobs live under `backup:` in `engram.yaml` (see
`engram.yaml.example`). The **secrets never go in `engram.yaml`** — they live in
`~/.config/engram/daemon.env` (mode 600), beside the Neo4j password and Telegram
token:

| env var | meaning |
|---|---|
| `ENGRAM_BACKUP_PASSWORD` | restic encryption password |
| `ENGRAM_BACKUP_S3_KEY_ID` | B2/S3 application key id |
| `ENGRAM_BACKUP_S3_KEY` | B2/S3 application key |
| `ENGRAM_BACKUP_REPO` | optional: an explicit restic repo string, overriding the endpoint/bucket composed from the config |

> **Lose the password and the backups are unrecoverable.** That is what
> client-side encryption buys you, and it is not reversible. Store it in a
> password manager the moment the installer prints it.

### Operating it

`bin/engram_backup.py` is operator-runnable as well as daemon-called:

```
engram_backup.py status      # configured? reachable? latest snapshot?
engram_backup.py init        # create the repository (idempotent)
engram_backup.py backup      # snapshot + prune
engram_backup.py snapshots   # list, newest first
engram_backup.py check       # verify repository integrity
engram_backup.py restore --snapshot latest --target DIR
```

**Restore never touches live data.** It stages into a directory (default
`~/.claude/restore-<timestamp>/`) for you to review and move into place
yourself — restoring memories in place would be destructive.

### What it backs up

`~/.claude/projects/*/memory/` (the `.md` stores), each tenant's `vault:` (minus
`.obsidian/` and `.trash/`), `engram.yaml`, and `graph/.env`. Transcripts
(`*.jsonl`) and the rebuildable indexes are excluded.


## SilverBullet (browser editor over the vaults)

The vaults are server-side markdown; [SilverBullet](https://silverbullet.md) lets
you edit them from any browser on the tailnet without a sync layer, because it is
server-first — it serves the directory directly. engram keeps indexing the same
files; SilverBullet is just the human's editor. `./install.sh --silverbullet`
stands it up; `silverbullet/README.md` is the full guide.

- **One instance per tenant**, each mounting only its own vault AND on its own
  Docker network — so a compromised or mistaken editor can reach neither another
  identity's files nor another instance over the network. (Each still has
  ordinary outbound internet egress, which SilverBullet's Library sync needs.)
- **Tailnet-only.** Instances bind `127.0.0.1:3011..3014`; the Traefik gateway
  gives each a `wiki-<tenant>.home.arpa` hostname with TLS. No public exposure,
  no Tailscale funnel.
- **Reachable from atlas.** The atlas dashboard's **Wiki** nav item opens the
  SilverBullet vault of the currently selected tenant in a new tab, and the
  **Tenants** tab lists each tenant's vault with an "Open wiki" link. atlas builds
  `https://wiki-<tenant>.<suffix>` where `<suffix>` is `ui.wiki_host_suffix` in
  `engram.yaml` (default `home.arpa`) — set it to match the `SB_HOST_SUFFIX` used
  at install. It is a link to a separate origin with its own `SB_USER` login, not
  an embed, so the two keep independent sessions.
- **Work vaults read-only by default** (`SB_READ_ONLY`) — an EDITOR-layer control:
  it stops edits through the SilverBullet UI, but root inside the container can
  still write the bind-mounted vault. It is a posture, not a filesystem lock; a
  hard guarantee would need a kernel read-only mount with a separate writable
  path for SilverBullet's auth state, which this setup does not do. Remove the
  flag in `silverbullet/docker-compose.yml` to make a tenant read-write.
- **Auth** is SilverBullet's own `SB_USER` (with lockout), stored per tenant in
  `~/.config/engram/silverbullet.env` (mode 600). Traefik adds TLS, not a second
  password; a commented basicAuth middleware is available for defense-in-depth.
- The only file SilverBullet writes into a space is `.silverbullet.auth.json` — a
  dotfile the wiki walker ignores and the backup excludes, so nothing leaks into
  recall or needs configuring.

**Known interactions, stated plainly:**
- The image is pinned by digest (`:v2@sha256:…`); re-verify SilverBullet's
  on-disk footprint when you bump it, since the "only a dotfile" property was
  established empirically against that digest.
- `SB_USER` is passed as a container environment variable, so it is visible to
  anyone with Docker API/root access (`docker inspect`). That is inherent to
  SilverBullet's auth; the 0600 env file only protects it at rest from ordinary
  users.
- If you use SilverBullet's **Library** (downloaded templates/plugs land as `.md`
  under `Library/`) or let it create a space `CONFIG.md`, those are ordinary
  markdown and engram WILL index them. If that pollutes recall, exclude them with
  SilverBullet's `SB_SPACE_IGNORE` or keep them out of the vault root.
- The vault is bind-mounted `:z` (shared SELinux label) because BOTH the
  container and the host-side engram daemon/backup touch it; a private `:Z` label
  would lock the host processes out. On an Enforcing host, confirm the daemon can
  still read the vault after the first `up`.

## Web authentication (atlas + SilverBullet)

engram's own binaries have **no auth and need none** — the API (`127.0.0.1:8787`),
the CLI, the MCP server and the daemon are loopback/local and trusted. The thing
that needs protecting is the **web surface**: the atlas dashboard is published at
`engram.home.arpa` through the Traefik gateway, and its config editor can rewrite
`engram.yaml`, so over the LAN it must require a login. `./install.sh --web-auth`
provisions one.

**One credential, two enforcement points, web-only:**

- **atlas** → a Traefik `engram-auth` basicAuth middleware (bcrypt htpasswd,
  the same scheme the gateway's other routers use), attached to the existing
  `engram` router. This is the auth engram itself lacked.
- **SilverBullet** → each instance's `SB_USER` is set to the *same*
  `user:password`, so one credential logs into both. SilverBullet is not double-
  fronted by the gateway middleware, so there is no second prompt.
- **Web only** — all of this is at the gateway and the web apps. The loopback
  API, CLI, MCP and daemon never cross it and stay unauthenticated by design.

This is **shared credentials, not single sign-on**: the same user:password is
enforced separately by Traefik (atlas) and SilverBullet (its own `SB_USER`), with
no shared session — logging out of one does not affect the other. Traefik
basicAuth has no lockout; SilverBullet's own login does. Rotate by re-running
`--web-auth`, which also force-recreates any running SilverBullet instances so
the new login takes effect.

The credential is provided once (or generated) and stored in
`~/.config/engram/silverbullet.env` (mode 600) as `ENGRAM_WEB_USER` /
`ENGRAM_WEB_PASSWORD`, plus the `SB_USER_*` lines SilverBullet reads. It is never
printed to the installer output. Note that, as with any basicAuth, `SB_USER` is
visible to anyone with Docker/root access (`docker inspect`); the file mode
protects it at rest from ordinary local users.

Re-run `--web-auth` to rotate: it overwrites the htpasswd and the `SB_USER_*`
lines in place (and backs up the gateway's `tls.yml` before touching the router).
