---
description: Walk through memory files one-by-one with the user to keep, edit, or delete each. Reviews are user-driven — never silently modify or delete.
argument-hint: optional filter — "stale" / "all" / "type=feedback" / "name=foo"
---

You are guiding the user through a careful review of their memory files at `~/.claude/projects/<slug>/memory/`. The user decides every action. You never delete or rewrite a memory without their explicit "delete" or "edit" instruction for that specific file.

## Step 1 — pick the review set

Compute the slug from `$PWD` (convention: `/home/foo/bar` → `-home-foo-bar`; if `$PWD` is `$HOME` or `/`, slug = `-home-<user>`). Memory dir: `~/.claude/projects/<slug>/memory/`.

Parse `$ARGUMENTS`:
- empty or `all`  → review every `*.md` except `MEMORY.md`
- `stale`         → run `~/.claude/memory_audit.sh --filenames-only` and review only the flagged files
- `type=<type>`   → review only files whose frontmatter `type:` matches (read first ~10 lines to check)
- `name=<glob>`   → review only files whose name matches the glob

If you computed the set with `memory_audit.sh`, also run `~/.claude/memory_audit.sh` (full report) once at the top and show the user the summary before starting the walkthrough so they know *why* each file was flagged.

## Step 2 — present total + first batch

Before any per-file question, tell the user: "Reviewing N files. I'll show you each one with a summary, then ask what to do. Type `quit` at any prompt to stop and resume later."

## Step 3 — for each memory, one at a time

For each file in the review set:

1. Read the file (`Read` tool).
2. Print a compact card:
   ```
   ─── filename.md ────────────────────────────────
   type:        <from frontmatter>
   description: <from frontmatter>
   size:        <bytes>, mtime: <YYYY-MM-DD>
   flagged for: <reason if from audit, else "manual review">

   Summary: <2-3 sentence summary of the body in your own words>
   Indexed in MEMORY.md: <yes/no — and the exact index line if yes>
   ```
3. Use `AskUserQuestion` with these options:
   - **Keep as-is** — move on, no change.
   - **Edit** — user describes the change; you apply it with `Edit` (or rewrite the whole file with the same filename via `save_memory.sh` to keep the GitHub copy synced).
   - **Delete** — call `~/.claude/delete_memory.sh <filename>` (handles local file + GitHub remote + MEMORY.md line).
   - **Skip for now** — note in a session list, move on; we'll surface skipped files at the end.

4. If user chose **Edit**: ask "What should change?" with `AskUserQuestion` (single open question via Other). Apply the change. If the edit changes the description, also update the matching line in `MEMORY.md` and push via `save_memory.sh` (it handles both file + index).

5. If user chose **Delete**: run `~/.claude/delete_memory.sh <filename>` via Bash. Confirm the output shows both "Removed local" and "Updated MEMORY.md index" (or note if either was already absent).

## Step 4 — final report

After the last file (or when user types quit), print:

```
Review complete.
  kept:    N
  edited:  N  (filenames)
  deleted: N  (filenames)
  skipped: N  (filenames — re-run /memory-clean-review name=<file> later)
```

## Hard rules

- Never delete a file without showing its content first and getting an explicit "Delete" selection for *that* file.
- Never batch-delete from a list. One file → one prompt → one action.
- Never modify `MEMORY.md` directly — let `save_memory.sh` and `delete_memory.sh` keep it in sync with the repo.
- If `delete_memory.sh` fails for a file, stop the walkthrough and report; don't continue.
- Don't pre-summarize all N files before starting — generate each card *only* when its turn comes, so the user can quit early without wasted tokens.

$ARGUMENTS
