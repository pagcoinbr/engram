---
name: memory-tidy
description: Use when the user asks to clean up, tidy, consolidate, organize, or de-duplicate their memories / memory store — e.g. "tidy my memories", "clean up my memory", "consolidate memories", "merge duplicate memories". Triggers engram's auto-consolidation pass on demand instead of waiting for the weekly schedule.
---

Run engram's consolidation pass NOW (it normally runs weekly via the daemon):

    python3 ~/.claude/memory_auto_curate.py --apply

What it does — and how to report it back to the user:
- **Merges near-duplicate memories** into a compressed umbrella. Merges are *reviewed*
  (Codex if installed, else a one-tap Telegram approval) and *reversible* — absorbed
  sources go to `.quarantine/` for a 30-day probation with a one-tap UNDO.
- **Proposes stale orphans** (old, never-recalled, no merge target) for pruning via the
  Telegram approval gate — never auto-deleted; approved prunes go to `.trash/` (90-day undo).

It respects `auto_curate.enabled` and `auto_curate.review_gate` in `~/.claude/engram.yaml`
(if `enabled: false`, tell the user to flip it or run with the flag). After it runs,
summarize what was auto-merged (with the undo window) and what was queued for their approval.
Nothing is destroyed without a long recovery window.
