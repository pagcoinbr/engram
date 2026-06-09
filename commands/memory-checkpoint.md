---
description: Review this session and save any new memorable facts to your-org/engram-memory
argument-hint: (optional focus hint, e.g. "focus on the deploy flow")
---

Checkpoint the current session's memorable facts into the persistent Claude memory system at `your-org/engram-memory` (via `~/.claude/save_memory.sh`).

## Step 1 — read what's already there

Before deciding what's new, read the current project's memory index and files so you don't duplicate or overwrite blindly:

- `ls ~/.claude/projects/` to find the slug for `$PWD` (convention: `/home/foo/bar` → `-home-foo-bar`).
- `cat ~/.claude/projects/<slug>/memory/MEMORY.md` for the index.
- Read individual memory files relevant to what this session covered.

If a new fact extends or contradicts an existing memory, plan to **update** that file (reuse the filename — `save_memory.sh` handles the SHA for in-place updates) rather than creating a parallel entry.

## Step 2 — pick what's worth saving

Save only:
- Completed projects / features (what was built, where files live, why).
- Decisions with lasting impact (architecture, config choices, migrations).
- New infrastructure (scripts, crons, systemd units, hooks) and where to find them.
- User feedback and preferences (corrections, validated non-obvious choices).
- Non-obvious facts about the deployed environment that future sessions can't derive from code (perms, chain, runtime paths).

Never save:
- Temporary debugging steps or WIP.
- Anything already captured in the existing memories from Step 1.
- Info derivable from current code or `git log`.
- Ephemeral session state.

## Step 3 — save each memory

For each memory, pipe frontmatter+body into `save_memory.sh`. The script writes to `~/.claude/projects/<slug>/memory/` locally AND pushes to the GitHub repo. It also appends to `MEMORY.md` automatically if the filename is new.

Write the body in the **Summary → numbered Index → Body** shape so the file is graspable at a glance (see `[[memory_file_format]]`): a 2–4 sentence `## Summary`, a `## Index` numbered list of the body's section titles, then one `## <n>. <Title>` section per index entry. For `feedback`/`project` types, keep the `**Why:**` / `**How to apply:**` lines inside the body sections. A small one-fact memory may use just `## Summary` + body if a numbered index would be a single line.

```bash
cat <<'EOF' | ~/.claude/save_memory.sh <filename>.md "<one-line description for MEMORY.md>"
---
name: <short human name>
description: <one-line relevance hint for future-you>
type: <user|feedback|project|reference>
---

## Summary
<2–4 sentence quick-look at the whole memory>

## Index
1. <Section title>
2. <Section title>

## 1. <Section title>
<body>

## 2. <Section title>
<body>
EOF
```

Filename conventions: `<type>_<topic>.md` (e.g. `project_wizard_flow.md`, `feedback_no_trailing_summaries.md`, `reference_grafana_dashboard.md`).

## Step 4 — report

In ≤3 lines: list filenames saved or updated, filenames skipped and why (usually "already covered"), and a final "nothing new worth saving" if that's the case. No narrative.

$ARGUMENTS
