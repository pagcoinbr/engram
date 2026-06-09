---
description: Umbrella-building consolidation pass over the memory store — cluster narrow/overlapping facts, merge into class-level umbrella memories, prune stale ones. Dry-run by default; mutations only after explicit approval.
argument-hint: empty/"dry-run" (preview, default) | "apply" | optional scope e.g. "scope=project" or "cluster=server-a"
---

You are running as the memory **CURATOR** — a periodic consolidation pass over the persistent Claude memory store. This is an **UMBRELLA-BUILDING consolidation pass, not a passive audit and not a duplicate-finder**. (Ported from Hermes' background skill curator; adapted to this file-based memory store + the `save_memory.sh`/`delete_memory.sh` tooling.)

The goal of the memory collection is a **library of class-level facts and experiential knowledge**. A collection of dozens of narrow entries where each captures one session's specific detail is a *failure* of the library, not a feature. Recall matches on `description`, not on exact filename — so one broad umbrella memory with labeled subsections beats five narrow siblings for discoverability.

This command complements the others:
- `/memory-checkpoint` *appends* new facts.
- `/memory-clean-review` walks files one-by-one for *manual* keep/edit/delete.
- `/memory-to-skill` *promotes* a procedure-rich memory into an invokable skill.
- `/memory-curate` (this) *consolidates* the whole store into umbrellas, AND now **surfaces promotion candidates** (Step 3.5) so one weekly sweep both de-sprawls the store and graduates its runbooks into skills. Run it occasionally (e.g. weekly, or after several checkpoints).

## Step 0 — locate the store

Compute the slug from `$PWD` (`/home/foo/bar` → `-home-foo-bar`; if `$PWD` is `$HOME` or `/`, slug = `-home-<user>`). Memory dir: `~/.claude/projects/<slug>/memory/`. Index: that dir's `MEMORY.md`.

Parse `$ARGUMENTS`:
- empty or `dry-run` → **PREVIEW MODE** (default): do everything below except mutate. Your report is the deliverable.
- `apply` → live mode: still produce the plan first, then mutate only after the Step 4 approval gate.
- `scope=<type>` → restrict candidates to that frontmatter `type` (e.g. `scope=project`).
- `cluster=<keyword>` → restrict to memories whose name/description contains the keyword (e.g. `cluster=billing`).

## Step 1 — load the landscape

Read `MEMORY.md`, then read the first ~12 lines (frontmatter + lead) of every `*.md` candidate. Build a table: filename, `type`, `description`, mtime, and whether it's indexed in `MEMORY.md`.

**Protected (pinned-equivalent) — do NOT consolidate, prune, or rewrite without explicit per-item user approval:**
- `type: user` (identity) and `type: feedback` (behavioral guidance) memories. These are the "pinned" set. Skip them by default; only touch one if the user explicitly names it.

## Step 2 — find umbrella clusters

Scan all candidates. Identify **prefix/domain clusters** (memories sharing a first token or domain keyword). In this store expect clusters like: `project_acme-api_*`, `project_billing_*`, `reference_*` (topology / endpoints), `feedback_*`, `project_*_deploy`, etc.

For each cluster with 2+ members, **do not ask "are these pairs overlapping?"** — ask **"what is the umbrella CLASS these all serve? Would a maintainer write that as one memory with N labeled subsections?"** If yes, pick or create the umbrella and absorb the siblings.

Hard judgment rules (do not violate):
1. **Pairwise distinctness is the wrong bar.** "Each has a distinct trigger/description" is NOT a reason to keep separate — it's a reason to make each a labeled subsection under the umbrella.
2. **Never delete outright as the first move.** Absorbed content must live on in the umbrella before its source file is removed. Removal is recoverable only via the `your-org/engram-memory` git history — treat it as the maximum destructive action.
3. **Never touch protected (`user`/`feedback`) memories** without explicit approval (Step 1).
4. `keep` is legitimate ONLY when a memory is already a class-level umbrella and no proposed merge would improve recall. "Narrow but distinct" → move it under an umbrella, don't keep it standalone.

## Step 3 — the three consolidation moves

Per cluster, choose the right one:

- **a. MERGE INTO EXISTING UMBRELLA** — one member is already broad enough. Rewrite it (same filename) via `save_memory.sh`, adding a labeled subsection for each sibling's unique fact. Then remove the absorbed siblings (Step 4).
- **b. CREATE A NEW UMBRELLA** — no member is broad enough. Write a new class-level memory (`<type>_<class>.md`) whose body has short labeled subsections covering the shared topic, absorbing the siblings' facts. Cross-link finer memories with `[[name]]`. Then remove the absorbed siblings.
- **c. FOLD DETAIL INTO THE BODY** — a sibling holds narrow-but-valuable session detail. Move that detail into a labeled subsection (or a `### references` block) of the umbrella body, preserving the `**Why:**`/`**How to apply:**` structure. Then remove the sibling.

Preserve format exactly: top-level frontmatter `name` / `description` / `type`, then the Summary → numbered Index → Body shape (`## Summary` 2–4 sentences, `## Index`, then one `## <n>. <Title>` section per entry; see `[[memory_file_format]]`). For an umbrella, make each absorbed sibling its own numbered index entry + body section, keeping the `**Why:**` / `**How to apply:**` lines for `project`/`feedback` inside those sections. Keep the umbrella's `description` recall-friendly (mention each absorbed sub-topic so searches still hit).

## Step 3.5 — surface promote-to-skill candidates

Consolidation makes memories broader; separately, some memories are **repeatable procedures backed by real tools/scripts** that deserve to become invokable skills (an lncli/bitcoin-cli runbook, a stuck-withdrawal reconcile job, a deploy guardrail). While the whole store is loaded, flag these too. Run the deterministic ranker (read-only):

```bash
python3 ~/.claude/memory_promote_candidates.py
```

A memory is a promotion candidate when the ranker marks it `eligible: yes` (status ∈ {corroborated, fixed}, frequency ≥ FREQ_MIN, procedure_score ≥ PROC_MIN) **and** it does not already carry a `**Promoted to skill:**` marker. Apply the same sanity filter the promotion flow uses — SKIP backups of already-existing commands/scripts and pure inventory/topology (facts belong in memory, not a skill).

**Do NOT draft skills inline here.** List the top eligible candidates in the Step 5 `promotions:` block; in apply mode, hand each approved one to the dedicated flow — `/memory-to-skill memory=<name>` (or follow `~/.claude/commands/memory-to-skill.md`), which runs its own draft + human gate + source back-pointer. Consolidation and promotion stay **separate actions with separate gates**, and promotion never deletes the source memory.

## Step 4 — approval gate, then mutate (live mode only)

In **PREVIEW MODE**, stop here and emit the Step 5 report. Do not call `save_memory.sh` or `delete_memory.sh`.

In **apply** mode:
1. Show the full plan (the Step 5 structured block) and the count of writes / removals.
2. Use `AskUserQuestion`: **Apply all consolidations** / **Apply consolidations but confirm each pruning** / **Pick clusters** / **Cancel**. If Step 3.5 flagged promotion candidates, add a **Promote flagged memories to skills** option (or defer them to item 6).
3. For each approved **consolidation**: first write/patch the umbrella via `save_memory.sh` (verify it succeeds and the content is present), *then* `~/.claude/delete_memory.sh <sibling>.md` for each absorbed file.
4. For each **pruning** (stale, no merge target): one file → one explicit confirmation before `delete_memory.sh`. Never batch-delete prunings.
5. Never edit `MEMORY.md` by hand — `save_memory.sh`/`delete_memory.sh` keep it and the GitHub repo in sync. After mutating, re-read `MEMORY.md` and confirm absorbed entries are gone and umbrellas are present.
6. For each approved **promotion** (Step 3.5): run `/memory-to-skill memory=<name>` and let that flow's own gate handle the skill write + the source-memory back-pointer. A promotion **never** deletes the source memory — it stays the source of truth and keeps recording the fact.

## Step 5 — structured report (required)

Emit a human summary of clusters processed and decisions left alone, **then** this block (downstream-parseable; distinguishes merges from prunes):

```yaml
consolidations:
  - from: <old-memory>.md
    into: <umbrella-memory>.md
    reason: <one short sentence — why merged, not just "similar">
prunings:
  - name: <memory>.md
    reason: <one short sentence — why removed with no merge target>
promotions:
  - name: <memory>.md
    skill: <suggested-skill-name>
    reason: <one short sentence — the repeatable procedure it encodes>
```

Every file you **removed** MUST appear in exactly one of `consolidations`/`prunings`. `promotions` is separate — those memories are NOT removed (they get a skill + a back-pointer). Leave any list empty (`promotions: []`) if none. In preview mode this is the deliverable ("actions I *would* take"); in apply mode it records what was done. If you end a pass having processed fewer than the obvious clusters, you stopped too early — go back.

## Reset the consolidation-due reminder

A `SessionStart` hook (`~/.claude/memory_curate_check.sh`) automatically nudges "🧠 Memory consolidation due" when the store has grown (≥10 new memories) or it's been ≥7 days since the last pass — mirroring Hermes' "idle + interval" trigger, surfaced for approval (it never mutates).

After a completed pass — an `apply`, **or** a dry-run you've reviewed and decided needs no changes — reset that counter so it stops nagging:

```bash
~/.claude/memory_curate_check.sh --mark-done
```

Thresholds are tunable via `CLAUDE_CURATE_THRESHOLD` / `CLAUDE_CURATE_INTERVAL_DAYS`.

## Hard rules (summary)

- Dry-run is the default. Mutations require the Step 4 gate.
- Protected `user`/`feedback` memories are off-limits without explicit per-item approval.
- Content survives in the umbrella *before* any source file is removed; removal is recoverable only via git history.
- Prunings (no merge target) are confirmed one-by-one — never batch-deleted.
- Promotion (Step 3.5) is a separate action with its own gate via `/memory-to-skill`; it never deletes the source memory (which keeps the fact + gets a back-pointer).
- Only `save_memory.sh` / `delete_memory.sh` may touch `MEMORY.md`.

$ARGUMENTS
