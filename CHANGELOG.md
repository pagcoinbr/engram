# Changelog

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
