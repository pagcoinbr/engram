---
description: Promote a fixated memory (a high-trust, frequently-recalled memory that encodes a repeatable tool/script procedure) into a first-class Claude Code skill under ~/.claude/skills/. Ranks candidates with memory_promote_candidates.py, lets Claude draft the SKILL.md, gates every write through the human, and leaves a back-pointer on the source memory. Dry-run by default.
argument-hint: empty/"dry-run" (default) | "apply" | "memory=<name>" | "cluster=<keyword>"
---

You are running the memory **PROMOTION** pass: turning a *fixated* memory into a
reusable **skill**. The idea (from the operator): a memory earns trust over time
(`/memory-fixate` graduates it suspect → provisional → corroborated → fixed). Some
trusted, frequently-recalled memories don't just record a fact — they encode a
**repeatable procedure backed by real tools/scripts** (an lncli/bitcoin-cli runbook,
an emergency-unlock script, a reconcile job). Those deserve to stop being passive
prose and become an invokable skill with its scripts bundled alongside.

The deterministic ranking is done by `~/.claude/memory_promote_candidates.py`, which
consumes `~/.claude/memory_score.py --json` for fixation status/frequency/confidence
and adds a procedure-density score. **Claude drafts the SKILL.md prose directly** (skill
instructions are delicate and Claude-facing) — but every filesystem write is gated by
the human, and dry-run is the default.

## Step 0 — scope

Parse `$ARGUMENTS`: empty/`dry-run` ⇒ preview only (no mutation); `apply` ⇒ scaffold
the skill after the gate in Step 4; `memory=<name>` ⇒ promote that specific memory
(`.md` optional); `cluster=<keyword>` ⇒ consider a prefix/keyword family as one skill.
The store path is resolved by the ranker (canonical, `$PWD`-independent).

## Step 1 — rank candidates

Run `python3 ~/.claude/memory_promote_candidates.py` (add `--memory <name>` if scoped,
or `--json` if you want the structured signals). Show the ranked table. Each row has:
`promote_score`, `procedure_score`, `frequency`, `confidence`, `status`,
`suggested_skill_name`. The gate is `status ∈ {corroborated, fixed}` AND
`frequency ≥ FREQ_MIN` AND `procedure_score ≥ PROC_MIN`. Already-promoted memories
(carrying the `**Promoted to skill:**` marker, or whose skill dir exists) are excluded.

## Step 2 — select & read

Pick the named memory, or propose the top candidate(s) and ask which to promote.
Then `Read` its full body (and any sibling cluster members if `cluster=`).

Before drafting, sanity-check the candidate — skip / flag these:
- **Already-a-tool backups.** Memories like `reference_security_audit_command.md` or
  `reference_setup_server_creds.md` are *backups of an already-existing command/script*.
  The tool already exists; there is nothing to promote. Say so and pick another.
- **Pure inventory/topology** (server lists, address tables) with no runnable steps —
  the procedure floor usually catches these, but confirm there's a real *action* to
  encode, not just facts. If it's facts, it belongs in memory, not a skill.

## Step 3 — draft the SKILL.md (Claude writes this directly)

Produce a skill folder layout in your head: `~/.claude/skills/<suggested_skill_name>/`
containing `SKILL.md` and, if the memory embeds scripts, `scripts/<name>.sh|.py`.

The `SKILL.md` frontmatter + body:
- `name:` = the suggested kebab-case name.
- `description:` = one line a future Claude will match on — **lead with the trigger**
  ("Use when the user asks to …"), name the servers/tools involved, and what it does.
  This is the only thing loaded until the skill fires, so make it precise.
- Body = the procedure as **numbered, runnable steps** lifted from the memory, with the
  exact commands. Preserve the operator's known gotchas. Reference the bundled scripts
  by relative path. End with a short "Source" line linking the origin memory
  (`<store>/<memory>.md`) so the memory stays the source of truth.

Extract any inline shell/python from the memory body into `scripts/` files rather than
leaving them only in prose, so the skill is self-contained and runnable.

## Step 4 — GATE (always, before any write)

Show the user:
1. The proposed directory tree (`skills/<name>/SKILL.md`, `scripts/…`).
2. The full SKILL.md you drafted, and each script file's contents.

**No secrets baked in.** Skill files are durable, plaintext, and may be synced. Never
embed credentials, `.env` values, macaroons, private keys, or tokens — reference them
the way the memory does (read at runtime via the operator's rules; see
`[[feedback_sensitive_files_via_ollama]]`). If the source memory contains a secret,
replace it with a `<placeholder>` + a note on where the real value lives.

Then `AskUserQuestion`: **Create skill** / **Edit first** (apply their changes, re-show)
/ **Cancel**. Never write a skill file without explicit approval. Default to Cancel if
the user is unsure.

## Step 5 — apply (apply mode only, after approval)

- `mkdir -p ~/.claude/skills/<name>/scripts` and write `SKILL.md` + script files.
- `chmod +x` every script under `scripts/`.
- Leave a **back-pointer on the source memory** so it won't be re-promoted and stays
  canonical. Append to the memory body:
  `\n**Promoted to skill:** <name> (<YYYY-MM-DD>) — see ~/.claude/skills/<name>/`
  and save it via `~/.claude/save_memory.sh <memory>.md "<existing one-line description>"`
  piping the updated content on stdin. (`save_memory.sh` is the **only** thing allowed
  to touch the store + `MEMORY.md`.) Do **not** copy the procedure out of the memory —
  the skill references it; the memory keeps recording the fact.

## Step 6 — verify

Confirm `~/.claude/skills/<name>/SKILL.md` exists and its frontmatter parses (name +
description present). Re-run `python3 ~/.claude/memory_promote_candidates.py --memory
<memory>` and confirm it now reports the memory as **already promoted / ineligible**.
Remind the user that newly created skills are discovered at the **start of the next
session** (the skill list is loaded then), so `/<name>` becomes available after a restart.

## Hard rules

- Dry-run is the default; Step 4 gates every filesystem write.
- **Claude** drafts the SKILL.md/scripts; the **human approves** before anything lands.
- No secrets in skill files — reference, never embed.
- The source memory stays the source of truth: the skill links back, the memory gets a
  back-pointer; the procedure is not duplicated and the original is never deleted.
- Only `save_memory.sh` touches the memory store + `MEMORY.md`.
- Don't promote backups of already-existing commands/scripts (Step 2).

$ARGUMENTS
