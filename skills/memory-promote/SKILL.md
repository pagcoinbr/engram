---
name: memory-promote
description: Use when the user asks to turn a specific EXISTING memory or procedure into a skill, or to promote a stored memory to a skill — e.g. "make that memory a skill", "promote memory X to a skill", "turn this stored procedure into a reusable skill". (For a procedure they describe fresh in chat, harvest already tags it automatically — this is for an already-stored memory.)
---

Promote an existing memory on demand by stamping it `promote: requested`. That bypasses only
the ~3-week maturity wait — every safety gate (provenance, static scan, adversarial review)
AND the Telegram approval still apply, and a memory with no runnable steps still won't become
a skill.

1. Find the target file in `~/.claude/projects/<slug>/memory/` (ask the user which memory if
   it's ambiguous; the slug is `$HOME` with `/`→`-`).
2. Add `promote: requested` to its frontmatter `metadata:` block (a new line under `type:`),
   leaving the rest of the file unchanged.
3. Tell the user it will be drafted and **proposed for one-tap approval on Telegram** on the
   next hourly pipeline tick — or run it now:

       python3 ~/.claude/memory_skill_autoinstall.py --apply

Never write a skill file directly — the promotion pipeline + the Telegram approval handle
the install (into `skills/auto/<name>/`), with the artifact hash-bound so nothing unreviewed
goes live.
