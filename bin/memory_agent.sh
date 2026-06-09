#!/usr/bin/env bash
# memory_agent.sh — Stop hook: spawns a background Claude agent to review the
# session and save any genuinely NEW memories to the canonical claude-memory
# store. Dedup-aware — the agent reads MEMORY.md + existing entries first.
# Only fires if a recent session transcript exists. Debounced to 10 min, which
# also guards against the background `claude -p` recursively re-triggering us.

source "${HOME}/.claude/memory_lib.sh" 2>/dev/null || true

# Never run inside a spawned subagent (prevents recursive claude -p storms).
[[ -n "${MEMORY_SUBAGENT:-}" ]] && exit 0

TRANSCRIPT="${CLAUDE_TRANSCRIPT_PATH:-}"
LOCK="/tmp/memory_agent.lock"

# Canonical store (independent of the CWD this hook happens to run in).
if command -v memory_dir >/dev/null 2>&1; then
    MEM_DIR="$(memory_dir)"
    MEM_REPO="$(memory_repo)"
else
    MEM_DIR="${HOME}/.claude/projects/$(printf '%s' "$HOME" | sed 's|/|-|g')/memory"
    MEM_REPO="${CLAUDE_MEMORY_REPO:-}"
fi

# Only run if transcript is available and recent
if [[ -z "$TRANSCRIPT" ]] || [[ ! -f "$TRANSCRIPT" ]]; then
    exit 0
fi

# Debounce: don't run more than once per 10 minutes (also stops recursion)
if [[ -f "$LOCK" ]]; then
    LOCK_AGE=$(( $(date +%s) - $(stat -c %Y "$LOCK") ))
    [[ $LOCK_AGE -lt 600 ]] && exit 0
fi
touch "$LOCK"

# Run memory agent in background so it doesn't block the session
(
    claude -p "$(cat << PROMPT
You are a memory agent. Review the conversation transcript below and decide if any genuinely NEW facts should be saved to the persistent memory store at ${MEM_DIR} (GitHub: ${MEM_REPO}).

FIRST — dedup before saving anything:
  1. Read ${MEM_DIR}/MEMORY.md (the index of existing memories).
  2. For each candidate fact, skim the related existing memory file(s). If it is already covered, do NOT create a duplicate. Update the existing file (reuse its filename) only for a material correction.

Save memories ONLY for:
- Completed projects or features (what was built, where files live)
- Decisions with lasting impact (architecture, config choices)
- New infrastructure (scripts, crons, services installed)
- Feedback or preferences the user expressed

Do NOT save:
- Temporary debugging steps
- Anything already covered by an existing memory (you checked the index)
- Incomplete or abandoned work
- Anything derivable from current code or git history

Format each memory as top-level frontmatter (name / description / type) then the body; for project/feedback types add **Why:** and **How to apply:** lines. Save via:
  echo "<frontmatter+body>" | ~/.claude/save_memory.sh <type>_<topic>.md "<one-line description>"
save_memory.sh pushes to GitHub and updates MEMORY.md automatically.

If nothing new is worth saving, do nothing.

TRANSCRIPT:
PROMPT
)
$(tail -400 "$TRANSCRIPT" 2>/dev/null)" \
    --allowedTools "Bash(~/.claude/save_memory.sh*),Bash(echo*),Read,Grep,Glob" \
    2>/dev/null
) &

exit 0
