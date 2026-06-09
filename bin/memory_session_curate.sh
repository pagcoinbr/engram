#!/usr/bin/env bash
# memory_session_curate.sh — Stop hook. Fires ONCE when the current session first
# exceeds session_curate.min_minutes (memory_ai.yaml), launching a background
# subagent (claude -p) that curates memories using the LOCAL Ollama models via the
# ollama MCP. Bounded by a per-session marker + a daily cap. Never applies
# destructive changes. End-of-turn trigger: it measures real elapsed time and runs
# on a warm session whose new memories are already captured.

set -uo pipefail

# Never run inside a spawned subagent (prevents recursion via the subagent's Stop).
[[ -n "${MEMORY_SUBAGENT:-}" ]] && exit 0

source "${HOME}/.claude/memory_lib.sh" 2>/dev/null || true
AI="${HOME}/.claude/memory_ai.py"

TRANSCRIPT="${CLAUDE_TRANSCRIPT_PATH:-}"
[[ -n "$TRANSCRIPT" && -f "$TRANSCRIPT" ]] || exit 0
SID="${CLAUDE_SESSION_ID:-$(basename "$TRANSCRIPT" .jsonl)}"

# Fast path: already curated this session -> exit before any python.
MARK="/tmp/mem_session_curate.${SID}.done"
[[ -f "$MARK" ]] && exit 0

# Gates (config).
[[ "$(python3 "$AI" --get local_enabled 2>/dev/null)" == "true" ]] || exit 0
[[ "$(python3 "$AI" --get session_curate.enabled 2>/dev/null)" == "true" ]] || exit 0
MINM="$(python3 "$AI" --get session_curate.min_minutes 2>/dev/null)"; [[ "$MINM" =~ ^[0-9]+$ ]] || MINM=10
MAXDAY="$(python3 "$AI" --get session_curate.max_per_day 2>/dev/null)"; [[ "$MAXDAY" =~ ^[0-9]+$ ]] || MAXDAY=4

# Session duration from the first transcript timestamp.
START=$(python3 - "$TRANSCRIPT" <<'PY'
import json, sys, datetime
for line in open(sys.argv[1], errors="ignore"):
    try:
        ev = json.loads(line); ts = ev.get("timestamp")
        if ts:
            print(int(datetime.datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp())); break
    except Exception:
        continue
PY
)
[[ "$START" =~ ^[0-9]+$ ]] || exit 0
DUR=$(( ($(date +%s) - START) / 60 ))
(( DUR >= MINM )) || exit 0

LOG="${HOME}/.claude/logs/fixation"; mkdir -p "$LOG"

# New-memory gate: skip unless enough memories were saved since the last pass.
# (time alone is the wrong signal — nothing new => nothing to curate.)
MINNEW="$(python3 "$AI" --get session_curate.min_new_memories 2>/dev/null)"; [[ "$MINNEW" =~ ^[0-9]+$ ]] || MINNEW=3
STATE="$(command -v memory_state_file >/dev/null 2>&1 && memory_state_file || echo "")"
NEW=0
if [[ -n "$STATE" && -f "$STATE" ]] && command -v jq >/dev/null 2>&1; then
    NEW=$(jq -r '.saves_since_curate // 0' "$STATE" 2>/dev/null); [[ "$NEW" =~ ^[0-9]+$ ]] || NEW=0
fi
if (( NEW < MINNEW )); then
    echo "[$(date -Iseconds)] session ${SID} ${DUR}min but ${NEW} new memories (< ${MINNEW}) — skip" >> "${LOG}/session_curate.log"
    exit 0
fi

# Daily cap.
DAYF="/tmp/mem_session_curate.$(date +%Y%m%d).count"
N=$(cat "$DAYF" 2>/dev/null || echo 0); [[ "$N" =~ ^[0-9]+$ ]] || N=0
(( N >= MAXDAY )) && exit 0
touch "$MARK"; echo $((N + 1)) > "$DAYF"

# Reset the new-memory counter so we don't re-curate until more memories arrive.
if [[ -n "$STATE" && -f "$STATE" ]] && command -v jq >/dev/null 2>&1; then
    tmp="$(mktemp)" && jq '.saves_since_curate = 0' "$STATE" > "$tmp" 2>/dev/null && mv "$tmp" "$STATE" || rm -f "$tmp"
fi

if command -v memory_dir >/dev/null 2>&1; then MEM_DIR="$(memory_dir)"; else
    MEM_DIR="${HOME}/.claude/projects/$(printf '%s' "$HOME" | sed 's|/|-|g')/memory"; fi
echo "[$(date -Iseconds)] session ${SID} ${DUR}min, ${NEW} new memories -> launching curation subagent ($((N+1))/${MAXDAY} today)" >> "${LOG}/session_curate.log"

# Background subagent: offloads heavy text work to local Ollama via MCP.
(
  MEMORY_SUBAGENT=1 claude -p "$(cat <<PROMPT
You are a background memory-curation subagent. Offload ALL heavy text work to the LOCAL Ollama models via the ollama MCP tools (mcp__ollama__ollama_code with self_verify=true, and mcp__ollama__ollama_run) — do not spend your own tokens rewriting memories. Memory store: ${MEM_DIR}.

Run a LIGHT pass (read-only except the built-in suspect quarantine):
1. Bash: python3 ~/.claude/memory_fixate_cron.sh   (runs the Duplicate Finder + Injection Guard, quarantines injection-suspects, drafts distillations; writes a report).
2. Read ~/.claude/logs/fixation/latest/REPORT.md.
3. Using a local model via mcp__ollama__ollama_code, review the proposed distillations for any that are clearly safe and lossless, and the lowest-confidence memories for obvious staleness.
4. Append a short "## Subagent review" section to that REPORT.md with your recommendations for the human. DO NOT merge or delete memories. DO NOT trust quarantined/suspect memories. End with a one-line summary.
PROMPT
)" \
    --allowedTools "Bash(python3 ~/.claude/*),Bash(~/.claude/*),Read,Grep,Glob,mcp__ollama__ollama_code,mcp__ollama__ollama_run,mcp__ollama__ollama_list_models" \
    >>"${LOG}/session_curate.log" 2>&1
) &
exit 0
