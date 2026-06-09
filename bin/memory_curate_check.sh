#!/usr/bin/env bash
# memory_curate_check.sh — SessionStart hook: nudge a consolidation pass when
# the memory store has grown or it's been a while since the last /memory-curate.
# Mirrors Hermes' curator "idle + interval" trigger, surfaced for approval —
# it never mutates memory itself.
#
# Usage:
#   memory_curate_check.sh             # (SessionStart) print a reminder if due
#   memory_curate_check.sh --mark-done # reset counter + stamp now (call after a curate run)
#
# Tunables (env): CLAUDE_CURATE_THRESHOLD (default 10 new memories),
#                 CLAUDE_CURATE_INTERVAL_DAYS (default 7).

set -uo pipefail
source "${HOME}/.claude/memory_lib.sh" 2>/dev/null || exit 0

STATE="$(memory_state_file)"
THRESHOLD="${CLAUDE_CURATE_THRESHOLD:-10}"
INTERVAL_DAYS="${CLAUDE_CURATE_INTERVAL_DAYS:-7}"

if [[ "${1:-}" == "--mark-done" ]]; then
    mkdir -p "$(dirname "$STATE")"
    printf '{"schema":1,"last_curate_at":"%s","saves_since_curate":0}\n' "$(date -Iseconds)" > "$STATE"
    rm -f "${HOME}/.claude/logs/fixation/.unread" 2>/dev/null || true
    echo "[memory] curate state reset (consolidation marked done)"
    exit 0
fi

# Reminder mode — best-effort, never fail a session start.
command -v jq >/dev/null 2>&1 || exit 0
[[ -f "$STATE" ]] || exit 0

saves=$(jq -r '.saves_since_curate // 0' "$STATE" 2>/dev/null || echo 0)
last=$(jq -r '.last_curate_at // empty' "$STATE" 2>/dev/null || echo "")

due=0; reason=""
if [[ "$saves" =~ ^[0-9]+$ ]] && (( saves >= THRESHOLD )); then
    due=1; reason="${saves} new memories since last consolidation"
fi
if [[ -n "$last" ]]; then
    last_epoch=$(date -d "$last" +%s 2>/dev/null || echo 0)
    if (( last_epoch > 0 )); then
        days=$(( ( $(date +%s) - last_epoch ) / 86400 ))
        if (( days >= INTERVAL_DAYS )); then
            due=1; reason="${reason:+$reason; }${days}d since last consolidation"
        fi
    fi
fi

if (( due )); then
    echo "🧠 Memory consolidation due (${reason}). Run /memory-curate to review and merge — dry-run by default, nothing changes without your approval."
fi

# Surface an unread weekly fixation report from the cron timer, if any.
UNREAD="${HOME}/.claude/logs/fixation/.unread"
if [[ -f "$UNREAD" ]]; then
    rp="$(cat "$UNREAD" 2>/dev/null)"
    echo "📋 Weekly memory-fixation report ready: ${rp:-$UNREAD} — run /memory-fixate apply to act on it (suspects already quarantined)."
fi
exit 0
