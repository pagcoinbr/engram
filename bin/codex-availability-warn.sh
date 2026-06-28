#!/usr/bin/env bash
# codex-availability-warn.sh — SessionStart hook.
# Makes the otherwise-silent Codex fail-open VISIBLE on the TUI:
#  1. If last night's unattended memory apply ran while Codex was unavailable
#     (gate failed open), warn once and clear the marker.
#  2. If the Codex advisor is unavailable right now, warn that the pre-commit/
#     deploy review gate is failing open (commits + nightly apply go unreviewed).
# Read-only, fast, fail-quiet. Plain stdout is surfaced at session start.
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"

# Never run from a headless/subagent session (e.g. the nightly planner's own claude -p):
# it must not CONSUME the morning markers before the human's interactive session sees them.
[[ -n "${MEMORY_SUBAGENT:-}" ]] && exit 0

# Honour an alternate install home (ENGRAM_BIN/ENGRAM_CLAUDE_HOME), else this
# script's own dir, else the default ~/.claude.
CLAUDE_HOME="${ENGRAM_BIN:-${ENGRAM_CLAUDE_HOME:-$(cd "$(dirname "$0")" && pwd)}}"
[[ -f "$CLAUDE_HOME/memory_ai.py" ]] || CLAUDE_HOME="$HOME/.claude"
ADVISOR="$CLAUDE_HOME/skills/code-advisor/scripts/code_advisor.py"
NIGHTLY_DIR="$CLAUDE_HOME/logs/fixation/nightly"
MARK="$NIGHTLY_DIR/.codex-was-down"
DEFER="$NIGHTLY_DIR/.deferred-for-human"

# 1. Nightly fail-open marker (one-shot) — Codex was UNAVAILABLE, gate ran blind.
if [[ -f "$MARK" ]]; then
  WHEN="$(cat "$MARK" 2>/dev/null)"
  echo "⚠️  Last night's memory apply ran WITHOUT Codex review (advisor was unavailable${WHEN:+, run $WHEN}). Review ~/.claude/logs/fixation/nightly/latest/REPORT.md"
  rm -f "$MARK" 2>/dev/null
fi

# 2. Deferred-for-human marker (one-shot) — Codex WORKED and flagged items the
# nightly agent did NOT apply; they await your manual decision this morning.
if [[ -f "$DEFER" ]]; then
  SUMMARY="$(cat "$DEFER" 2>/dev/null)"
  echo "🟡 Last night's memory maintenance DEFERRED ${SUMMARY:-some decisions} for your manual review (Codex/agent would not auto-apply). Act via /memory-curate — details: ~/.claude/logs/fixation/nightly/latest/REPORT.md"
  rm -f "$DEFER" 2>/dev/null
fi

# 2. Current Codex availability.
if ! timeout 12 python3 "$ADVISOR" --status 2>/dev/null | grep -qi "logged in"; then
  echo "⚠️  Codex advisor UNAVAILABLE — the pre-commit/deploy review gate is FAILING OPEN (commits + the nightly memory apply proceed UNREVIEWED). Fix: codex login status"
fi
exit 0
