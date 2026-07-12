# Running engram autonomously

engram can run its whole memory lifecycle **unattended** — harvest your Claude Code
sessions, graduate durable facts into recall, consolidate near-duplicates, and promote
proven memories to skills — while keeping the *risky, irreversible* steps behind a
one-tap human approval on Telegram.

The design principle is **reversibility-tiering**: an operation runs automatically if it
is reversible (there's an undo); it needs a human only when it's *irreversible or
behavioral*. So most of the system is silent-automatic, and your phone only buzzes for
the handful of ops that genuinely need you.

---

## 1. Prerequisites

- A working generation backend (`backend:` in `~/.claude/engram.yaml`):
  - `ollama` / `llama_cpp` — a local GPU box (free, private), **or**
  - `ccg` — route the `claude` CLI through a cc-gateway OAuth proxy (headless-safe), **or**
  - `claude` — raw `claude -p` (needs a logged-in session; flaky under systemd).
- The systemd daemon installed (`install.sh` with `DAEMON=systemd`) — it runs a
  maintenance pass on a timer and drains the approval queue.
- Optional but recommended: the **Qdrant vector store** (for dedup/consolidation) and
  the **Telegram approval gate** (for the risky ops).

## 2. The autonomy switches (`~/.claude/engram.yaml`)

| Flag | Off (default) | On (autonomous) |
|---|---|---|
| `daemon.intervals.maintenance` | `315360000` (~off) | `21600` (every 6h) — runs harvest→graduate→score |
| `auto_graduate.enabled` | `false` | `true` — durable `user-direct` facts enter recall (deterministic gates) |
| `auto_curate.enabled` | `false` | `true` — merge near-duplicates (Codex-reviewed, reversible) |
| `skill_autoinstall.enabled` | `false` | `true` — proven memories → skills (via approval) |

All gates are deterministic and **fail-closed**; the LLM is value-only, never a safety
gate. Turn them on one at a time and watch `~/.claude/logs/pipeline.log` for a cycle
before enabling the next.

## 3. The Telegram approval gate

The gate DMs you a one-tap **Approve / Reject** for irreversible ops and a one-tap
**Undo** for reversible ones. It uses long-poll (no webhook, no public endpoint).

1. In Telegram, message **@BotFather** → `/newbot` → copy the token.
2. Put it in `~/.config/engram/daemon.env` (mode 600) as:
   ```
   TELEGRAM_BOT_TOKEN=123456:AA...
   TELEGRAM_CHAT_ID=<your chat id>
   ```
   Message your new bot once; `engram_telegram_gate.py --poll` picks up your chat id
   from the first message if you don't set it.
3. Test: `engram_telegram_gate.py --notify "hello"` — you should get a DM.

The daemon drains the queue every 5 min. Proposals expire after **72h → dropped**
(never auto-applied). Everything works headless without a token too — approve locally
with `engram_telegram_gate.py --approve <id>`.

## 4. What runs where (the op tiers)

- **Silent auto** (reversible + deterministic): harvest, graduate, fixation scoring,
  graph/vector sync, injection-suspect quarantine, near-*lossless* merges.
- **Auto-apply + one-tap UNDO** (reversible judgment): near-lossless consolidations —
  sources go to `.quarantine/` (30-day probation, restorable), you get an Undo button.
- **Async APPROVE required** (irreversible/behavioral, 72h→drop): **skill installs**,
  **lossy/Codex-deferred merges**, permanent deletes, index rewrites.
- **Terminal-only** (never proposed remotely): `/memory-curate` (prune orphans,
  fact-check, non-dup distills), `/memory-clean-review`, and any change to the gates
  or thresholds themselves.

## 5. Trust, safety, and kill-switches

- **Weekly digest** on Telegram: what the daemon did autonomously, and what's pending.
- **Recoverability**: deletes → `.trash/` (90 days); merges → `.quarantine/` (30-day
  probation) then `.trash/`. Nothing is destroyed without a long undo window.
- **Codex** reviews every risky op before it applies; only what Codex can't clear
  reaches your phone.
- **Kill-switches**: set any `*.enabled: false` (takes effect next tick);
  `touch ~/.claude/skills/auto/.disabled` to freeze skill installs;
  `systemctl --user stop engram.timer` to pause the daemon entirely.

## 6. The slash commands under autonomy

Harvesting, scoring, and near-dup merges are automatic — you don't run commands for
them. The `/memory-*` commands remain as the **manual console for the judgment ops
autonomy deliberately won't do unattended**:

- `/memory-curate` — prune stale orphans, fact-check corrections, non-duplicate
  consolidations, review/delete quarantined suspects. **Keep this.**
- `/memory-clean-review` — periodic one-by-one human audit.
- `/memory-checkpoint` — capture the *current* session now (and assistant-provenance
  facts auto-graduate won't take).
- `/memory-to-skill` — manual skill promotion (the gate handles the auto path).

`/memory-fixate` is **retired** — its scoring is now automatic and its consolidation is
handled by auto-curate + `/memory-curate`.
