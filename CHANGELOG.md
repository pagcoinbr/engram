# Changelog

## Unreleased

### Added
- **`/memory-cluster`** — topic consolidation. Merges many memories describing ONE
  system into a single distilled file, resolving contradictions as it goes. This is a
  different job from `auto_curate`, which only merges near-duplicates (cosine ≥ 0.92,
  same `type`, ≤2/run); complementary facets of one system score far below that and were
  never eligible. Includes three gates learned the hard way: a **coherence** gate (a
  shared name prefix is not a topic — merging unrelated subsystems degrades recall), a
  **type** gate (`feedback`/`snippet`/`user` must stay standalone, or a snippet silently
  leaves the `memory_snippet_lookup` shelf), and a **size** gate.
- Documents the wiki-link rewrite trap: `'\[\[' + '|'.join(names) + '\]\]'` is wrong —
  alternation binds loosest, so middle names match as bare text anywhere in a file and
  first/last names leave dangling brackets. The command mandates a capture group and a
  post-rewrite corruption check.
- Safety ordering is load-bearing and explicit: the merged file is **saved, indexed and
  verified before any source is deleted**. `save_memory.sh` can refuse (secret guard) or
  its push can fail; deleting first would leave the store with neither the sources nor
  the replacement. Every path is bound to an absolute `$MEM` so a glob can never touch
  the caller's own repository, and each rewritten memory is persisted through
  `save_memory_content_only.sh` so a configured remote cannot diverge from local.
- **Per-memory locking in the canonical writers.** The index lock serialised `MEMORY.md`,
  but nothing serialised the memory *files*: `save_memory.sh` overwrote and
  `delete_memory.sh` removed with no compare-and-swap, so two writers touching the same
  filename (a session save racing the unattended curator, or a long `/memory-cluster`
  run) could lose an update outright. `memory_lib.sh` gains
  `memory_file_lock_acquire` / `_release` / `memory_file_sha`, and
  `save_memory.sh`, `save_memory_content_only.sh` and `delete_memory.sh` each hold that
  memory's own lock (under `.locks/`) for their whole mutation. Lock order is documented
  and enforced: **file lock outer, index lock inner**.
- **`MEMORY_NOCLOBBER=1`** (`save_memory.sh`) — create-only. Refuses when the destination
  exists, checked *inside* the lock so it is atomic against a racing creator. An
  overwrite gets no `.trash` snapshot, so callers that mean "this must be new" can now
  demand it. Default behaviour is unchanged.
- **`MEMORY_EXPECT_SHA=<sha256>`** — conditional write/delete, honoured by both
  `delete_memory.sh` and `save_memory_content_only.sh` (the latter matters because a
  read-transform-write pass reads *before* the writer takes its lock). A malformed value
  is rejected rather than silently treated as "no CAS requested".
- **`MEMORY_EXPECT_REMOTE_SHA=<blob sha>`** (`delete_memory.sh`) — remote CAS. The local
  hash only proves *this* copy is unchanged; another seat can publish an update remotely
  that was never read. The remote sha is resolved and compared **before anything local is
  touched**, and the DELETE then uses that exact sha, so a moved blob or an unreachable
  API aborts with the local canonical file still in place — checking after the local `rm`
  would leave the store showing only the merged memory while an updated record sat in
  `.trash`.
- Locks **fail closed**. `memory_file_lock_acquire` returns non-zero on open or
  acquisition failure and every caller aborts — unlike the index lock, which falls back to
  running unlocked. That fallback is defensible for a best-effort index append but not
  here: a compare-and-swap that proceeds unlocked after a timeout silently stops being
  one. `MEMORY_LOCK_WAIT` (default 30s) tunes the timeout; with `flock` absent, a caller
  that explicitly asked for CAS is refused rather than quietly downgraded.
  All of the above are opt-in; default behaviour is unchanged.
- The merge is composed in a mode-600 temp file **outside** the store and piped to
  `save_memory.sh`, which is the guarded writer. Writing the canonical file first and
  scanning afterwards inverts the secret guard: a rejected key is already in the store
  and recallable. Composing in `$TMP` means a rejection leaves the store untouched.
- The source set is validated before use — `MEMORY.md`, `MEMORY_FULL.md`, hidden/control
  files, non-direct-children and non-`.md` names are hard-rejected. A broad argument
  would otherwise hand the index itself to `delete_memory.sh`.
- Money, custody and authorization contradictions may **not** be resolved by `mtime`.
  Checkout, restore, rsync and migration rewrite timestamps without touching meaning, so
  "newest wins" can make a stale payout address canonical and delete the correct one.
  Those require live verification or an explicit user decision; absent either, both
  values are kept, labelled, and the conflict marked unresolved.
- The store is resolved through `memory_lib.sh`'s `memory_dir` — **not** from `$PWD` —
  so it is byte-identical to what `save_memory.sh`/`delete_memory.sh` target. A box
  typically hosts many stores under `~/.claude/projects/`; a mismatch would verify one
  store while deleting same-named files from another.
- The link-rewrite pass now hands new content to `save_memory_content_only.sh` instead
  of writing files itself, so each rewrite is serialized by that memory's lock,
  secret-scanned, and pushed — no truncation window and no local/remote divergence.

### Fixed
- `save_memory_content_only.sh` pushed to the remote unconditionally. On a local-first
  install (no `CLAUDE_MEMORY_REPO`) `_gh_put_file ""` fails, so the script exited 1 on
  **every** call even though the local write had succeeded — making its exit status
  useless to any caller that checks it. Now guarded like `save_memory.sh`.

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
