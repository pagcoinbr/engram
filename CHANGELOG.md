# Changelog

## Unreleased — fix the 24/7 graph re-harvest loop (per-tenant ingest state)

The local model was being called around the clock. Root cause: the graph ingest's
on-disk state — the extraction cache, `insert_state.json` (the per-file "done" set)
and `sync_state.json` — was a single **flat** copy shared by every store, and
`graph_sync.py` invoked `memory_graph_insert.py` **without telling it which slug**
it had just extracted for. The inserter fell back to the `$HOME`-derived default
slug, looked for another store's `.md` under the wrong `MEM_DIR`, hit
`skip (no .md)`, never stamped "done", and the extractor re-harvested those memories
on the next cycle — forever. Only the one store that happened to match the default
(`-root`) ever completed; every tenant's memories churned the model endlessly
(measured: 2324 generation attempts for 113 distinct prompts, same ones re-sent
~24×, `insert_state.json` frozen for days).

### Fixed

- **Per-slug ingest state** — new `graph/mg_state.py` scopes the extraction cache,
  `insert_state.json` and `sync_state.json` under `state/<slug>/`. `graph_sync.py`,
  `memory_graph_insert.py` and `graph_maint.py` all resolve them from the active
  slug, so each store tracks its own "done" set and a memory is extracted once.
- **Slug propagation** — `graph_sync.py` now passes `--slug` (and `CLAUDE_MEMORY_SLUG`)
  to the insert child, so the inserter's `MEM_DIR` and state match the store that
  was extracted. This was the actual root cause; the scoping made it correct and
  collision-free (two tenants with a same-named file no longer share one "done").
- **One-time migration** — `graph_sync.py --migrate-state` (also run lazily, flock-
  guarded + idempotent) splits any legacy flat state into per-slug dirs, assigning
  each done file to the store it was inserted from (sha-disambiguated) so files
  already in the graph are not re-inserted (which would duplicate episodes).
- `tests/test_graph_ingest_scope.py` — pins per-slug path scoping, slug resolution,
  and the migration split/disambiguation/idempotency (no graphiti/Neo4j needed).

Side note unchanged here: the `:8090` generation server flaps up/down; while it is
down, extraction cannot make progress (clean "nothing extracted"), and when it is
up each store now ingests once and goes quiet.

## Unreleased — atlas tenant awareness + per-tenant Wiki link

atlas knew nothing about tenants: the Scope selector listed raw project slugs with no
hint of which tenant owned them, there was no view of a tenant's collections/group/
vault, and the per-tenant SilverBullet vaults were unreachable from the dashboard.

### Added

- **`GET /api/atlas/tenants`** (`atlas_api.py`) — the configured tenants and, for each,
  the stores (slugs) it owns, its Qdrant collections, Graphiti group, vault, memory
  count, and its SilverBullet URL (`wiki-<name>.<suffix>`). Non-fatal import of
  `engram_tenant.py` (mirroring the audit loader); degrades to an untenanted payload so
  atlas stays usable with no `tenants:` block. The Rust API is single-tenant per process
  and lists no tenants, so this is Python-only.
- **Atlas "Tenants" tab** — one card per tenant (collections, graph group, vault, slugs,
  count) with an "Open wiki" link.
- **Scope selector grouped by tenant** — projects nest under their owning tenant
  (`<optgroup>`), with a trailing "Unassigned" group; untenanted installs keep the flat
  list. Each project in `/api/atlas/projects` and every snapshot now carries a `tenant`
  field.
- **Per-tenant "Wiki" nav link** — opens the selected tenant's SilverBullet vault in a
  new tab (a separate origin with its own login — a link, not an embed); falls back to
  the Tenants view when no tenant is resolvable from the current scope.
- **`ui.wiki_host_suffix`** config key (default `home.arpa`, matches install's
  `SB_HOST_SUFFIX`) — documented in `engram.yaml.example` and `CONFIG.md`.
- `tests/test_atlas_tenants.py` — pins the tenant-model contract the tab reads and the
  wiki-URL shape.

## Unreleased — LLM call audit + atlas "LLM Calls" tab

There was no record of what engram sent to the generation model, so a loop (the same
prompt re-sent over and over) was invisible. This adds a content-free audit of every
generation call and a UI tab that streams them and flags exact repeats.

### Added

- **`bin/engram_llm_audit.py`** — append-only JSONL at `~/.claude/logs/llm_events.jsonl`,
  one line per generation *attempt*: `proc`, `backend`, `model`, `role`, `kind`
  (generation|health), `call_id`/`attempt`, `chars`, `ms`, `outcome`, and a **keyed
  HMAC digest** of the prompt (`~/.config/engram/llm_audit.key`, mode 600) — a repeat is
  detectable without storing any prompt text (the log is global across tenants). Writes
  are best-effort (never raise into a generation), multi-process-safe (lock-before-open
  rotation at 16 MB → `.1`, backward `tail`). `detect_loops()` flags identical
  `(digest,model,role,proc)` groups, excluding health probes.
- **Instrumentation (generation only, Python):** the `engram_llm.py` backends
  (`_llamacpp_generate`, `_ollama_generate`, `_claude_generate` per retry, `_ccg_generate`
  preflight), the native `ollama_stream` path in `memory_distill_verified.py`, and the
  Graphiti `chat.completions.create` wrapper in `mg_config.py` (now installed always, for
  logging; reasoning-injection stays conditional).
- **`GET /api/atlas/llm`** (`atlas_api.py`) — recent events + flagged loops; non-fatal
  import, schema-versioned, `limit` capped, degrades to "unavailable".
- **Atlas "LLM Calls" tab** — header metrics (gen calls / 5 min, distinct prompts, repeat
  ratio), a Possible-loops panel (exact-repeat detector; health excluded), and a recent-
  calls table polling every 5 s.
- `tests/test_llm_audit.py`; `tests/run_all.sh` points `ENGRAM_LOG_DIR` at a throwaway dir
  so the suite never pollutes the real audit log.

Reviewed by codex (APPROVE-WITH-CHANGES; all points folded in).

## Unreleased — one recall path (stop the duplicate embedding calls)

Recall was exposed by three MCP servers at once — `engram-graph` (Graphiti+vector+
keyword), `engram-vector` (vector/fused) and `engram-rust` (the Rust hybrid). The
two Python servers each ran their OWN legs, and `engram-graph`'s recall embedded the
query **twice per call** (its graph leg and its in-process Qdrant leg each called the
embedding model on the same text). On a local llama/bge-m3 endpoint that showed up as
every recall hitting the model in duplicate.

### Changed

- **`engram-rust` is now the single recall path.** `memory_recall` /
  `memory_recall_hybrid` are served only by the Rust `engram-mcp` server, which embeds
  the query once and degrades to keyword+vector when no graph is installed.
- **`engram-graph` no longer exposes recall.** Removed `memory_recall` /
  `memory_recall_hybrid` (and the now-dead `_recall_hybrid` / `_graph_ranked`). Keeps
  its graph-native tools: `memory_search_facts`, `memory_neighbors`, `memory_stats`.
- **`engram-vector` no longer exposes recall.** Removed `memory_vector_recall` and
  `memory_recall_fused`. Keeps raw inspection: `memory_vector_search`,
  `memory_vector_stats`.
- **`bin/memory_recall.py` (TUI/CLI) embeds once.** When the graphiti graph leg runs,
  the redundant Qdrant vector leg is skipped — mirroring the Rust compat path. Fast
  mode (no graph leg) still runs the vector leg. Verified: non-fast recall went from 2
  embedding calls to 1; fast stays at 1.
- `install.sh` no longer adds `qdrant-client` to the graph venv (graph recall is gone),
  and docs (README/CONFIG/ARCHITECTURE/engram.yaml.example/vector docs) point recall at
  `engram-rust`.

## Unreleased — shared web authentication (atlas + SilverBullet)

engram's own binaries have no auth and need none — the API (127.0.0.1:8787), CLI,
MCP and daemon are loopback/local. But the atlas dashboard is published through
the Traefik gateway at `engram.home.arpa` with NO auth, and its config editor can
rewrite engram.yaml — so over the LAN it was open. This adds one web credential,
shared with the SilverBullet editors, enforced only at the web layer.

### Added — `install.sh --web-auth`

- One `user:password`, provided once or generated, written to
  `~/.config/engram/silverbullet.env` (mode 600) and applied to two sinks:
  a Traefik `engram-auth` basicAuth middleware on the atlas router (the auth
  engram lacked), and every `SB_USER_*` (SilverBullet's own login), so a single
  credential logs into both with no double prompt.
- **Web only**: all at the gateway and the web apps; the loopback API, CLI, MCP
  and daemon are untouched by design, and a test asserts no request-auth gate is
  ever added to engram-app.
- `silverbullet/engram-web-auth.example.yml`, a CONFIG.md section, and
  `tests/test_web_auth.sh`.

### Reviewed by codex; blockers fixed

- **Missing gateway mount** — the gateway mounts individual files, not the parent
  dir, so a separate htpasswd would be invisible in the traefik container. The
  middleware now carries the bcrypt hash INLINE (`users:`), like the gateway's
  hermes router — no new mount.
- **Fail-open reporting** — if the gateway can't be secured (dir unwritable, no
  engram router, a commented tls.yml with no ruamel), it now says
  "WEB AUTH INCOMPLETE: atlas is NOT protected" and returns non-zero, instead of
  implying success.
- **Rotation not applied** — re-running `--web-auth` now force-recreates any
  running SilverBullet instances so the new SB_USER takes effect.
- **Secret handling** — the plaintext env is written via `mktemp` under
  `umask 077` and renamed, so it is never briefly world-readable.
- **Atomic gateway updates** — the middleware file and the tls.yml patch are
  written to a sibling temp and renamed; the router patch requires a successful
  backup first and refuses a lossy rewrite of a commented file (prefers ruamel,
  else prints the exact lines to add).

### Honest limitations (documented, not papered over)

Shared credentials, not SSO — no shared session, and Traefik basicAuth has no
lockout (SilverBullet's own login does). `SB_USER` is visible via `docker
inspect` to anyone with Docker/root access, inherent to basicAuth. No engram-app
code change — web auth lives entirely at the gateway and the web apps.

## Unreleased — SilverBullet: browser editor over the vaults

The vaults are plain markdown on a headless server, which is why "edit remotely"
kept needing a sync bridge (Obsidian is local-first; Obsidian Sync can't reach a
headless box). SilverBullet inverts that — a server-first editor that serves the
directory over the browser — so it edits the SAME files engram indexes, no sync
layer. engram stays the indexer and the agents' writer; SilverBullet is the
human's editor, and the loop closes: write a runbook in the browser, an agent
recalls it and files a finding into `_agent/`, you review that in the browser.

### Added

- **`silverbullet/`** — a compose file (one service per tenant, each mounting
  ONLY its own vault; loopback-bound; work tenants `SB_READ_ONLY`), a Traefik
  dynamic-config reference, and a README.
- **`install.sh --silverbullet`** — pulls the image, generates per-tenant
  `SB_USER` credentials into `~/.config/engram/silverbullet.env` (mode 600) and
  the Traefik routes (`wiki-<tenant>.home.arpa`, TLS, tailnet-only), and brings
  up the selected instances.
- `ARCHITECTURE.md` §4d-bis and a `CONFIG.md` section.

### Verified empirically against a live SilverBullet container

- SilverBullet edits plain `.md` in place — files seeded on disk by engram/agents
  are served unchanged, and pages created in the editor land as plain `.md`.
- The ONLY file it writes into a space is `.silverbullet.auth.json`, a **dotfile**
  the wiki walker already skips — confirmed in the real indexer (it indexed the
  human page and the agent's `_agent/` note, not the auth file). No `SKIP_DIRS`
  change was needed; a test pins the property, and the backup now excludes it.
- Mount-layer isolation: a tenant's container sees only `/data` (its vault);
  `/vaults` does not exist inside it, so another identity is structurally
  unreachable — the per-tenant boundary holding one level lower than the DB
  filters.

### Posture

Tailnet-only (loopback + Traefik over Tailscale, no public entrypoint, no
funnel); work vaults read-only by default (an editor-layer control, not a
filesystem lock); auth via SilverBullet's own `SB_USER`. Off unless
`--silverbullet` is passed. The Rust/engram pipeline is untouched beyond the
one-line walker test and the backup exclude.

## Unreleased — encrypted off-host backups (Backblaze B2 / any S3)

The source-of-truth data — the `.md` memory stores (~4 MB) and the Obsidian
vaults — had no off-host copy. This adds a daily, encrypted, retained backup of
exactly that authoritative set; the Qdrant/Neo4j indexes are left out because
they rebuild from it.

### Added

- **`bin/engram_backup.py`** — a restic wrapper, operator-runnable and
  daemon-called (the graph_sync.py shape). Subcommands `init` / `backup` /
  `snapshots` / `restore` / `check` / `status`. restic was chosen for one
  property above all: **client-side AES-256, always on** — the restore set
  includes `graph/.env`, so the store must never see plaintext — plus
  content-addressed dedup (a daily snapshot of an unchanged 4 MB set is nearly
  free), retention, and integrity checking.
- **A `backup` daemon task**, daily (86400s), beside `maintenance`. No-ops with
  one log line when disabled or when the password is absent; returns `False`
  (deferred, not stamped) on a transient failure so it retries next tick rather
  than waiting a full day. Its periodic `check` timer lives in the script's own
  state file, NOT the daemon state, because `tick()` rewrites the daemon state
  wholesale and would clobber a key written from inside a task.
- **`backup:` block** in `engram.yaml` (non-secret knobs only) and
  `./install.sh --backup` provisioning: ensures restic, prompts for
  bucket/endpoint/keys, generates and prints the restic password once if none is
  given, and inits the repo.

### Security properties, asserted by the round-trip test

`tests/test_backup_roundtrip.sh` (skips when restic is absent) seeds two memory
stores and a vault, backs up, deletes the source, restores to a staging dir, and
checks: the `.md`, vault and `graph/.env` files come back byte-for-byte;
`.obsidian/` and `.trash/` are excluded; the plaintext is **not findable in the
repo** (encryption is really in effect); and the restic password appears in
neither the log nor `engram.yaml`.

- **Secrets never touch `engram.yaml`.** The restic password and S3 keys live in
  `~/.config/engram/daemon.env` (mode 600), added to the installer's PRESERVED
  grep so a re-install keeps them. The restic env is built for the child process
  only — the daemon's own environment never carries it.
- **Restore never clobbers live data** — it stages into a directory for the
  operator to review and move deliberately.

### Scope

Memories + vaults + `engram.yaml` + `graph/.env`. Transcripts (`*.jsonl`) and
the rebuildable indexes are excluded. One whole-system repo. Daemon-only — the
Rust workspace is untouched. Off by default; dormant until an operator provides a
bucket.

## Unreleased — the Obsidian wiki corpus (stages 3-6)

A second corpus over a different shape of document, and the half of the feature
the tenancy work existed to make safe.

### Added

- **`engram-wiki` crate** — walk, parse, chunk. All pure: `walk` reads a
  directory, everything else transforms strings, so the chunker is unit-tested
  without a vault, a Qdrant or an embedding endpoint.
  - Recursive walk skipping `.obsidian` (the app's own state), `.trash`
    (indexing it would resurrect deleted pages in recall), `.git` and dotfiles.
  - Frontmatter (`title`, `aliases`, `tags`, inline and block lists), inline
    `#tags`, and all four wikilink forms: `[[Page]]`, `[[Page|alias]]`,
    `[[Page#Heading]]`, `![[Embed]]`.
  - Link resolution by Obsidian's rule — filename anywhere in the vault,
    shortest path winning — which needs a vault-wide index, not a path join.
  - **Code-fence awareness.** A technical wiki is full of `# comment` inside
    shell blocks; treating those as headings splits a page at every comment in
    every snippet and moves the boundaries whenever a sample is edited.
  - Heading-aware chunking, ~400 tokens with ~60 of overlap, splitting long
    sections on paragraph boundaries and never mid-character.
  - **Every chunk carries its breadcrumb into the embedding.** A chunk is
    retrieved alone, so "restart the broker and clear the queue" is
    indistinguishable between four runbooks without `Runbooks > RabbitMQ >
    Failover` in front of it. This matters more than the chunk size does.
  - An H1 repeating the page name is collapsed, since Obsidian pages routinely
    do that and `RabbitMQ > RabbitMQ > Queues` spends tokens saying nothing.
  - `CHUNKER_VERSION`, folded into the freshness hash, so a boundary change
    invalidates stored chunks instead of leaving documents looking current.
- **`engram-wiki-index`** — per-tenant, per-document freshness
  (`space · chunker version · content`), redaction before the embedding call,
  and a prune pass for documents that left the vault.
  - **Writes new chunks before trimming old ones.** An edit that shortens a page
    leaves orphan tail chunks — text no longer in the document but still
    answering queries — and deleting first would blank the page from recall for
    the duration of the re-embed. Write-then-trim is monotonic.
- **`wiki_search` / `wiki_fetch` / `wiki_write` MCP tools.** Search locates a
  section; fetch returns the surrounding document within a budget, which is the
  half that makes the feature useful — a truncated fragment is what the memory
  path already gives for a long document.
- **`engram_retrieval::rank`** over a new `Indexable` trait, so wiki chunks reuse
  BM25 rather than getting a second, subtly different implementation. `Memory`
  implements it; the memory path is unchanged.
- **A `wiki` daemon task**, per tenant rather than per store: a vault belongs to
  an identity, and several of a tenant's stores would each trigger a full
  re-walk of the same vault.

### Isolation

Wiki recall is a SEPARATE entry point from `recall`, not a leg inside it. Fusing
them would let a 40-chunk page outvote every memory in the store, and the prompt
hook injects from `recall` on every prompt. Wiki is retrieved when asked for.

Agent writes carry four independent guards: containment (the path must resolve
inside the vault, symlinks and `..` included), subtree confinement (and inside
`agent_subtree`, so curated pages cannot be touched), `.md`-only (a sync client
propagates whatever is in the vault), and redaction before the bytes hit disk —
not merely before indexing, because a credential in a synced vault has left the
box whatever the index holds. Writes are temp-file + fsync + rename, so Obsidian
never renders a half-written page.

### Fixed — found by running it

- **The MCP server refused to start for a tenant owning several stores.** It
  resolved the identity AND a default store up front, so a tenant whose stores
  did not match the launch directory got no server at all — making `wiki_search`
  unavailable because `memory_recall` could not have picked a default. The store
  is now resolved by the tools that need one; a slug failure surfaces only there.
- **A missing collection read as a transport failure.** A first-ever `--dry-run`
  died on Qdrant's 404, and the leg status reported "this vault has not been
  indexed yet" as `HTTP status client error`. A collection that does not exist
  holds no chunks.
- **An unconfigured vault returned empty results with puzzling leg statuses**
  rather than saying so; a setup step and a search that found nothing looked
  identical to a caller.

### Deployed

Four vaults at `/vaults/<tenant>` (`homelab`, `mjsv`, `bbhost`, `dseclab`), each
with `Notes/`, `Runbooks/`, an agent-writable `_agent/`, and a README describing
the layout and the isolation guarantee. Verified end to end: an agent filed a
finding, the incremental pass indexed only that document, search returned it as
the top hit, a canary planted in one vault was unreachable from another, and a
credential written by an agent reached disk redacted.

## Unreleased — multi-tenant agent identities (stage 3: deployment)

Applied to the live install: four identities (`bbhost`, `dseclab`, `mjsv`,
`homelab`), 13 stores, migrated and verified.

### Changed — the identity follows the store

The stage 1 rule, "declaring a tenant makes `--tenant` required on every binary",
was right about safety and wrong about where the boundary is. Deploying it
revealed three call sites it cannot work at:

- the prompt hook and the MCP server are registered ONCE in `settings.json`, so a
  fixed `--tenant` serves one of four identities;
- `memory_lib.sh` fires `engram-index --slug <slug>` backgrounded with its output
  discarded, so the refusal surfaced as memories silently no longer being indexed.

So the rule is now: **where a store can be named, the identity is determined.** The
slug→tenant mapping is operator-declared and total, so naming a store has exactly
one right answer and no way to pick wrongly. `--tenant` is needed only where no
store can be determined, and cross-checks when both are given. A store no tenant
claims is still refused rather than adopted, and there is still no default *tenant*
when none can be derived.

### Fixed — found by deploying it

- **`--group` to an old recall script corrupted the query**, rather than being
  ignored: its parser folded the flag's value into the search text, so "qdrant
  embedding space" became "qdrant embedding space canonical". Results still came
  back, for a polluted query, with visibly different ranking. The legacy path now
  sends no `--group` at all; a named tenant does, and the reply must echo the group
  it filtered on or the leg is refused. The regression test asserts the child's
  argv, because every fixture implements the new parser and none could see it.
- **`memory_graph_recall.py` imported `engram_tenant` before `mg_config` had set
  `sys.path`**, so the Graphiti child died with `ModuleNotFoundError` under a named
  tenant. The script now resolves its own module path instead of depending on
  import order.
- **`/api/v1/index/status` and `/api/v1/recall` disagreed** about which identity the
  server served: one resolved the tenant directly and refused on a tenanted
  install while the other derived it and worked. One process, two answers.
- **The engram-api unit sets no `HOME`**, so the environment-derived store was `-`
  (the slugification of `/`). A service has no project directory to derive an
  identity from; `install.sh` now writes `ENGRAM_TENANT` into `daemon.env` when a
  tenant owns the install's slug, and the live unit carries a drop-in.
- **`graph_sync.py` read one store from the environment and `graph_sync`/the
  wrapper dropped the scope**, so insert, export and reconcile all operated on
  whichever store `$HOME` named and reported success for the rest. All three are
  now per-tenant, with `--slug`/`--tenant` forwarded through
  `engram-graph-sync`.

- **The per-prompt budget starved the leg that was already finished.** The hook
  wraps the whole recall in `recall.inject.timeout_ms`, so a queued embedding
  request consumed the entire 2.5s and the prompt got NOTHING — discarding a BM25
  result computed in ~66ms from markdown on disk. This went from rare to routine
  with tenancy: the daemon now indexes one store per tenant, so the embedding
  endpoint sees far more load than when it owned a single store. Every prompt
  timed out at exactly 2.515s with zero results while a direct embedding call
  took 0.6s. The vector leg now gets its own deadline — a fraction of the prompt
  budget — so recall degrades to keyword + graph instead of to silence, and says
  so in the leg status.

### Migration result

- 71 Qdrant points copied to `engram_memory__homelab`, verified; source collection
  left intact, plus a Qdrant snapshot taken beforehand.
- 955 Graphiti nodes and 899 relationships relabelled `canonical` → `homelab`. A
  relabel rather than a re-insert because all of it derived from one store — the
  re-insert path remains as the refusal branch, verified by planting a colliding
  filename in two tenants' stores against the live graph.
- 5,239 pre-tenancy native nodes (no `slug`, no `tenant`) left in place, behind
  `--purge-native`. They are unreachable by every current query.
- All 12 cross-tenant store pairings refused; the hook injects each identity's own
  memories, derived from the session's project directory.

### Note

With `graph.backend: graphiti_compat` the keyword and vector legs are disabled by
design, so a tenant whose Graphiti group is not yet populated returns NO results
from `engram-recall` until its graph is synced. Before tenancy those queries hit
the shared group and returned another project's data, so this is the leak closing
rather than a regression — but the three new tenants need an index build (the
daemon now does this per tenant). The prompt hook is unaffected: it uses fast mode,
which runs BM25 over the markdown store.

## Unreleased — multi-tenant agent identities (stage 2: migration)

`engram-tenant-migrate` moves a pre-tenancy install onto tenants. Dry run by
default; refuses rather than guesses.

### What it does, and why each step differs

- **Qdrant** points are COPIED into the tenant's collection with their vectors
  carried across verbatim, not re-embedded. Re-embedding 71 points is cheap, but
  it is also how an index quietly changes meaning: the vectors came from whatever
  model was configured when they were written, and the job is to move them, not
  reinterpret them. Point ids are a uuid5 of `slug::file`, so a re-run upserts
  rather than duplicating, and the source collection is left intact for the
  operator to drop once recall is verified.
- **Graphiti** data is RELABELLED to the tenant's group — but only once exactly
  one tenant is shown to own the group's episodes. `Episodic` records a bare
  filename and no slug, so attribution is "which owned store holds a file by
  this name": one holder is normal, zero is a deleted memory, and two is
  undecidable. Entity nodes are shared across episodes, so an entity named by two
  identities is ONE node with ONE group that no update can divide — the tool
  refuses and points at a per-tenant re-insert from `graph/extractions/`.
- **Pre-tenancy native nodes** are DELETED, behind `--purge-native`. They carry
  neither `slug` nor `tenant`, and every native statement now requires both, so
  no query can reach them. The markdown store is authoritative; `--rebuild`
  regenerates the index.

### Corrected from the stage 1 notes

The earlier changelog said the Graphiti migration had to be a re-insert because
entity nodes are shared. That reasoning holds in general and does not apply to
this install: all 884 entities, 71 episodes and 899 relationships sit in
`canonical` and all derive from a single store, so there is nothing to split and
a relabel is exact and free. The re-insert path is still implemented as the
refusal branch, for the case where a group genuinely spans identities.

### Guards

- Refuses while any store on disk belongs to no tenant, and names them. A store
  no tenant owns has no collection to be written to, and adopting it into
  whichever tenant happens to be running would put one identity's memories inside
  another's boundary.
- Reports slugs declared in the config with no store on disk, which otherwise
  look like a tenant that simply has no memories yet.
- Verifies the destination point count against the source before reporting
  success, while the source is still intact.
- Migration Cypher is exempt from the per-statement tenant check — crossing
  groups is its purpose — so it carries a POSITIVE requirement instead: a test
  asserts no migration statement selects its rows by nothing, and that anything
  writing a group binds the group it writes. An exemption that only subtracts a
  check is how the earlier carve-out hid a live leak.

## Unreleased — multi-tenant agent identities (stage 1)

Groundwork for a multi-tenant Obsidian wiki corpus. This stage adds the tenant
model and its enforcement, and in doing so closes a cross-tenant leak that was
already live.

### Fixed — the graph leg crossed projects

- **Graphiti had no partition at all.** Every memory in every store was inserted
  with the single literal `group_id = "canonical"` (`graph/mg_config.py`), and
  `graph/memory_graph_recall.py` filtered by neither group nor slug. With 11
  populated stores sharing that group, a recall run from one project could return
  another project's facts. The group is now resolved per identity on both the
  read and write paths, and the 1-hop `LINKS_TO` neighbour query scopes BOTH ends
  of the hop — scoping only one would leak a neighbour's name through an edge.
- **The test that should have caught it had a carve-out that exempted it.** The
  Cypher scope guard in `crates/engram-graph` skipped any statement not
  containing `Engram`, commented "scoped by Graphiti itself" — which was false.
  Removed, and the guard is now per-PATTERN rather than per-statement.
- **`EngramEntity` nodes were global.** `MERGE (e:EngramEntity {name: name})`
  carried no slug, so entity nodes were shared across every store and identity.
  The old guard passed it because the surrounding statement mentioned `$slug`
  somewhere. Nothing read those nodes, so it never surfaced — it would have the
  moment anything traversed `MENTIONS` or `SUBJECT`. Found by the new guard.
- **`--only` swallowed the next flag's value** in `memory_graph_insert.py`: it
  collected every non-`--` argument after `--only`, so `--only a.md --tenant work`
  asked for a memory named "work" and reported nothing to insert.
- **`memory_graph_recall.py` folded flag values into the search query.** Its
  parser rebuilt the query from every argument not starting with `--`, excluding
  only `--k`'s value by a string comparison. Any other flag's value joined the
  query text. Now parsed properly, with value-taking flags declared.

### Added — tenants

- **`engram-tenant` crate and `bin/engram_tenant.py`**, the two halves of one
  model, pinned against each other by `tests/test_tenant_parity.py` — the same
  treatment the embedding fingerprint gets, for the same reason: Python writes
  the indexes and Rust reads them, and a disagreement is not an error but an
  index that looks empty.
- **A `tenants:` block** in `engram.yaml` mapping an identity to the memory
  stores it owns and the vault it reads. An absent block means tenancy is off and
  behaviour is unchanged — the upgrade path. Declaring even one tenant makes
  `--tenant` required on every binary, including when only one exists.
- **Isolation in the type system.** The configuration is a single shared file, so
  every process holds every vault path; discipline is not enough. A `Tenant` is
  the only thing that can name a collection, a graph group or a vault path, and
  a `GraphScope` — obtainable only from a `Tenant`, and refused for a slug it
  does not own — is required by every graph call. Omitting the scope is a compile
  error rather than a silent cross-tenant read.
- **A collection per tenant** (`engram_memory__<name>`, `engram_wiki__<name>`)
  rather than one collection plus a mandatory filter, so a cross-tenant read
  requires naming the other collection. `QdrantClient::from_config` is gone;
  there is no constructor that picks a collection by itself.
- **Vault containment**, canonicalized before the check so a symlink pointing at
  another tenant's vault is refused — a leak below the level any database filter
  can see. Agent writes are confined again, to `agent_subtree`.
- **`--tenant` on every binary**, and a per-tenant daemon loop with per-tenant
  error isolation, so one identity's unreachable backend does not stop the others.

### Changed — breaking

- **`graph.backend: native` indexes need a rebuild**: node identity now includes
  the tenant.
- **Moving existing stores into tenants requires a migration.** Qdrant points
  must be reindexed into the per-tenant collection and Graphiti episodes
  re-inserted under the new group — re-inserted, not relabelled, because Graphiti
  `Entity` nodes are shared across episodes and no `SET` can split one. Cached
  extractions in `graph/extractions/` mean this costs no LLM calls.
- **`engram-vector-search --space` is now required.** It was optional, which made
  searching without pinning an embedding space the shorter command — and that
  returns confident nonsense rather than an error.
- **`QdrantClient::search` takes a `Scope`** instead of two `Option`s. The space
  is no longer expressible as absent; it was an `Option` that one call site
  happened to fill in, which is a guarantee resting on a call site.

### Known limitations

- The legacy keyword leg rides Graphiti's `edge_name_and_fact` fulltext index,
  which cannot be partitioned — it ranks across every group and applies its limit
  before any group filter. The leg over-fetches 20× and filters, bounding the
  problem without solving it: a tenant holding under ~1/20th of the indexed edges
  can still be crowded out. The other legs do not share the defect, so recall
  degrades rather than leaking.
- `engram-app` serves one identity per process, from `--tenant` at startup. An
  operator wanting to inspect two tenants runs two instances or uses the CLI; a
  per-request tenant parameter would turn a loopback dashboard into a way to read
  any identity's memories over HTTP.

### Found by running it, not by testing it

Passing `--group` to the *installed* (older) `memory_graph_recall.py` did not get
ignored as assumed — its parser folded the value into the query, so a search for
"qdrant embedding space" became a search for "qdrant embedding space canonical".
Recall still returned results, for a polluted query, on every install that had not
refreshed its scripts. The legacy path now sends no `--group` at all (a new script
defaults to that group anyway); a named tenant does send it, and there the reply
must echo the group it filtered on or the leg is refused. No unit test could have
caught this — every fixture implements the new parser — so the regression test
asserts the child's argv rather than its records.

## Unreleased — `update.sh`

### Added
- **`update.sh`**, dry-run by default, `--apply` to act: a safe fast-forward pull, then
  `install.sh` with the daemon mode the box already uses. It restores
  `~/.claude/engram-local-overrides/`, rebuilds and restarts the Atlas and the Rust API
  when they are installed, and checks that the recall hook still injects memories. On
  failure it prints the rollback. Tested in `tests/test_update.sh`, isolated from the
  caller's real services.

## Unreleased — migrate the legacy graph to the Rust native backend

### Added
- **`engram-native-graph-sync --bootstrap-from-legacy`** seeds the native graph from
  the legacy Graphiti one **without calling a model**, where re-extracting a store
  on a local 8 GB GPU would take ~16 h. Check first that the legacy
  `fact_embedding`s are in the configured embedding space.
  - **Scoped:** it (and `--import-legacy-embeddings`) refuses unless the target is
    the `engram.env`-pinned store, failing closed with no pin. The legacy graph is
    one unscoped group built from that store.
  - **Verified:** a memory qualifies only if its file is byte-identical to one
    legacy episode (`Episodic.source_md`), and only that episode's facts are used,
    so an older ingested version never contributes.
  - **Redacted before writing:** facts are read out, redacted with the shared
    secret detector (text, entity names, relation name) and re-embedded from the
    redacted text in Rust; raw legacy facts never touch the native graph.
  - **History kept, never resurrected:** superseded facts (Graphiti
    `invalid_at`/`expired_at`) are imported with `valid_until`, so recall skips
    them; when a claim exists both live and superseded, the live copy wins.
  - **Atomic and resumable:** each memory's facts and commit marker are one
    transaction. An interrupted run leaves fact-less, unstamped nodes that the next
    run resumes; nodes carrying facts from elsewhere are left alone.

  Reference store (677 memories): 606 memories, 6,099 facts (106 redacted, 515
  historical) in ~30 s; the rest are left for the normal extracting sync.

### Fixed
- **Legacy recall served superseded facts as current.** Graphiti never deletes a
  contradicted fact, it stamps `invalid_at`/`expired_at`; no legacy read query
  (Python fast leg, Rust fast/semantic/keyword legs) checked either, so every
  version of history was recalled at once (632 of 6,720 edges on the reference
  store). All now require both to be null.
- **Both recall hooks redact graph facts before injecting them.** Facts reach the
  model verbatim, and legacy graph facts never passed the save-time secret guard.
  The Python hook injects nothing if its redactor cannot be imported.
- **The Rust recall hook injected no graph facts.** It printed only facts attributed
  to a returned memory; the fast leg's facts are unattributed (`Output::facts`).
  Same section and per-session dedup as the Python hook now.
- **Imported legacy facts were invisible to the fast leg,** which read triples
  only. Imported facts keep their edge's entity names and match them exactly, as
  the legacy query matched `Entity.name`.
- **Legacy graph facts reached every project.** The legacy graph is one unscoped
  group built from the `engram.env`-pinned store; both the Python and Rust fast legs
  now serve its facts only to that store. Native facts are slug-scoped already.
- **Extraction timeout comes from `llama_cpp.timeout_seconds`,** not a fixed 90 s
  that a local model (~85 s per extraction) failed at random.

## Unreleased — API authentication

### Security
- **Both local APIs require a token.** Every route on `engram-app` (:8787) except
  `/healthz`, and every route on `engram_api`/Atlas (:8765) except the page shell
  and `/login`, was unauthenticated, including config rewrites and memory
  create/delete. A loopback bind did not protect them from a browser tab
  (DNS rebinding). The token lives in `~/.claude/engram-api.token` (0600), created
  race-free by whichever server starts first; scripts use `Authorization: Bearer`,
  the browser gets an HttpOnly SameSite=Strict cookie from the `/login` POST form.
  The token is never accepted in a URL (history, proxy logs, Referer).
- **A config save that moves an endpoint drops that endpoint's `api_key`.** The
  writers preserved unmodelled keys, so repointing `llama_cpp.url`/`embed.url`
  sent the stored bearer token to the new host.
- **`/api/v1/recall?slug=` is validated.** It was joined straight into
  `projects/<slug>/memory`, so `../..` searched markdown anywhere on disk.
- **Atlas config backups and temp files are created 0600** instead of at the
  umask default, which left every credential world-readable.

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
