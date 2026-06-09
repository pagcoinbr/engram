---
description: Convert existing memories to the Summary → numbered Index → Body shape (feedback_memory_file_format) so each file is graspable at a glance. The prose rewrite runs on the LOCAL Ollama LLM (never Anthropic tokens, content stays on the LAN); facts are preserved, nothing is dropped, and every push is human-gated. Dry-run by default.
argument-hint: empty/"dry-run" (default) | "apply" | "memory=<name>" | "big-min=<chars>"
---

You are running the memory **REFORMAT** pass: rewriting existing memories into the
`## Summary` → `## Index` → body shape from `[[memory_file_format]]`, so a glance tells
you what a file holds. This is **restructuring, not distillation** — every fact in the
original must survive; you are only adding a summary + index and splitting the body into
numbered sections. The **language rewrite runs on the LOCAL LLM at localhost via the
Ollama MCP** (`mcp__ollama__ollama_code`, `qwen3.6:35b`) — never spend Anthropic tokens
rewriting memories, and the content stays on the LAN.

## Step 0 — scope

Parse `$ARGUMENTS`: empty/`dry-run` ⇒ preview only (no mutation); `apply` ⇒ push after the
gate in Step 4; `memory=<name>` ⇒ just that file (`.md` optional); `big-min=<chars>` ⇒
override the size floor. The store path is resolved by the lister (canonical,
`$PWD`-independent).

## Step 1 — list candidates

Run `python3 ~/.claude/memory_reformat_candidates.py` (add `--memory <name>` if scoped).
Candidates are memories that are big (`chars ≥ big-min`, default 1500) AND not already
formatted (missing `## Summary` or `## Index`), biggest first. Small one-fact memories are
left alone. Show the table; if unscoped, work top-down and tell the user how many remain.

## Step 2 — read the target

`Read` the full memory file (frontmatter + body). Keep the original frontmatter
(`name`/`description`/`type`) **verbatim** — you are not changing it. Note the existing
`MEMORY.md` one-line description (you'll reuse it on save).

## Step 3 — reformat on the LOCAL LLM (Ollama MCP, in the background)

Run the Ollama call **in the background** — wrap it in a general-purpose `Agent` spawned
with `run_in_background: true` (Ollama is slow/heavy; never block the session — see
`[[feedback_ollama_always_background]]`). Have the agent call **`mcp__ollama__ollama_code`**
with:
- `model: "qwen3.6:35b"`, `strip_code_fences: false`, `self_verify: true`, `num_ctx: 24576`,
  and `verify_json_schema` requiring
  `{ "reformatted_body": string, "index": [string], "dropped": [string], "notes": string }`.
- `prompt`: pass **only the body** (not the frontmatter) and instruct: *"Restructure this
  memory body into exactly this shape and return it as `reformatted_body`:
  a `## Summary` of 2–4 sentences giving the at-a-glance gist; a `## Index` numbered list of
  the section titles; then one `## <n>. <Title>` section per index entry holding the
  original content. FACT PRESERVATION: every command, code line, table CELL VALUE, port, IP,
  path, `[[wiki-link]]` and `**Why:**`/`**How to apply:**` line must be preserved exactly; do
  not add, remove, reword, or change spelling. You MAY normalise cosmetic markdown whitespace
  (e.g. table column padding) — only cell VALUES and code content must match. When you
  self-check, judge ONLY fact/command/value/wording preservation; cosmetic whitespace diffs
  are acceptable, not corruption. List in `dropped` anything you could not place (should be
  empty)."*

The "verbatim" trap: telling the model to copy tables byte-for-byte makes `self_verify` reject
harmless column-padding changes. Phrase it as cell-value/fact preservation + "spelling
unchanged" (catches the model's own typos) and let whitespace normalise.

If `mcp__ollama__ollama_code` is not loaded, `ToolSearch` for it; if still unavailable, ask the
user to restart the Ollama MCP — **never** shell out / curl-bypass the LAN LLM (see
`[[feedback_no_dodge_on_hook_deny]]`, `[[feedback_offload_to_ollama]]`).

## Step 4 — GATE (always, before any push)

Show the user the proposed `reformatted_body` and the model's `dropped` list. **If
`dropped` is non-empty, do not save** — the rewrite lost content; re-run or fix by hand.
Spot-check that commands/numbers/links survived. Then `AskUserQuestion`: **Save** / **Edit
first** / **Skip**. Never push without approval. Default to Skip if unsure.

## Step 5 — apply (apply mode only, after approval)

Reattach the **original frontmatter** on top of the approved `reformatted_body`, then push
via `~/.claude/save_memory.sh <file>.md "<existing one-line description>"` (stdin). That is
the **only** thing allowed to touch the store + `MEMORY.md`. Do not change the filename or
the frontmatter description.

## Step 6 — verify + next

Re-run `python3 ~/.claude/memory_reformat_candidates.py --memory <file>` and confirm it now
reports `fmt=yes` / not a candidate. If looping, report how many candidates remain and
continue to the next, or stop and let the user resume with `/memory-reformat` later.

## Hard rules

- Dry-run is the default; Step 4 gates every push.
- **Restructure, never distill** — no fact may be dropped; `dropped` must be empty to save.
- The **local LLM** does all prose (Ollama MCP, `qwen3.6:35b`); Claude only orchestrates.
- The Ollama call runs in the **background** (background Agent), never blocking the session.
- Frontmatter (`name`/`description`/`type`) and filename are preserved unchanged.
- Only `save_memory.sh` touches the store + `MEMORY.md`; changes are git-recoverable.
- Never bypass the Ollama MCP for the rewrite.

$ARGUMENTS
