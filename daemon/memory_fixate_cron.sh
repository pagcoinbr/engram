#!/usr/bin/env bash
# memory_fixate_cron.sh — 4x-daily memory maintenance (systemd timer).
#
# Time-of-day dispatch (schedule.claude_hours in memory_ai.yaml):
#   DAYTIME  → claude -p subagent: API-backed scoring + dedup review + report
#   NIGHTTIME → full local Ollama pipeline: score/quarantine/embed/distill/harvest
#
# Neither path merges or deletes memories unattended. Suspects quarantined (reversible).

set -uo pipefail
source "${HOME}/.claude/memory_lib.sh" 2>/dev/null || true
AI="${HOME}/.claude/memory_ai.py"
# Heavy/vector scripts (score, light-curate, distill) need qdrant-client + fastembed
# + numpy. Run them on the engram vector venv — the SAME interpreter the engram-vector
# MCP server uses — because the systemd timer's /usr/bin/python3 is PEP-668
# externally-managed and lacks those, which silently degrades the Duplicate Finder to
# cosine ("vector store unreachable"). Fall back to python3 if the venv is missing.
PYBIN="${HOME}/.claude/vector/venv/bin/python"
[[ -x "$PYBIN" ]] || PYBIN="$(command -v python3)"
LOGROOT="${HOME}/.claude/logs/fixation"
mkdir -p "$LOGROOT"
exec >>"${LOGROOT}/cron.log" 2>&1

# master local switch
if [[ "$(python3 "$AI" --get local_enabled 2>/dev/null)" != "true" ]]; then
    echo "[$(date -Iseconds)] local_enabled=false — skipping maintenance"; exit 0
fi

if command -v memory_dir >/dev/null 2>&1; then
    MEM_DIR="$(memory_dir)"; INDEX="$(memory_index)"
else
    MEM_DIR="${HOME}/.claude/projects/$(printf '%s' "$HOME" | sed 's|/|-|g')/memory"; INDEX="${MEM_DIR}/MEMORY.md"
fi
QUAR="${MEM_DIR}/.quarantine"
TS="$(date +%Y%m%d-%H%M%S)"; OUTDIR="${LOGROOT}/${TS}"
REPORT="${OUTDIR}/REPORT.md"; SCORES="${OUTDIR}/scores.json"
mkdir -p "$OUTDIR" "$QUAR"

# ── Time-of-day dispatch ─────────────────────────────────────────────────────
# Daytime (schedule.claude_hours): claude -p subagent — API reasoning, no Ollama.
# Nighttime: fall through to the full Ollama pipeline (harvest/distill/pipeline).
HOUR=$(date +%-H)
_CH="$(python3 "$AI" --get schedule.claude_hours 2>/dev/null || echo '[7,20]')"
C_START=$(echo "$_CH" | python3 -c "import json,sys; print(json.load(sys.stdin)[0])" 2>/dev/null || echo 7)
C_END=$(echo   "$_CH" | python3 -c "import json,sys; print(json.load(sys.stdin)[1])" 2>/dev/null || echo 20)
if (( HOUR >= C_START && HOUR < C_END )); then
    echo "[$(date -Iseconds)] daytime run (hour=${HOUR}) -> pre-compute Ollama, then claude -p analysis"
    (
        # Pre-run Ollama work in shell so claude -p only does text analysis (no tool calls to Ollama).
        # This avoids the pre-tool-use hook blocking writes to ~/.claude/ and keeps claude -p fast.
        "$PYBIN" "${HOME}/.claude/memory_score.py" --json > "$SCORES" 2>/dev/null \
            || { echo "[$(date -Iseconds)] ERROR scoring; skip daytime run"; exit 1; }
        "$PYBIN" "${HOME}/.claude/memory_light_curate.py" > "${OUTDIR}/curate.txt" 2>/dev/null

        # Feed pre-computed data to claude -p for analysis; capture report via stdout (no Write tool).
        SCORES_SNIPPET=$(python3 -c "import sys; print(open('${SCORES}').read()[:3000])" 2>/dev/null)
        CURATE_SNIPPET=$(head -100 "${OUTDIR}/curate.txt" 2>/dev/null)

        MEMORY_SUBAGENT=1 claude -p \
"Analyze this memory maintenance data and return ONLY a markdown report (no other text).
Do NOT call any tools — all data is provided below.

SCORES (JSON):
${SCORES_SNIPPET}

LIGHT CURATE OUTPUT:
${CURATE_SNIPPET}

Return a concise markdown report with these sections:
## Daytime maintenance — $(date -Iseconds)
## Status (one line)
## Merge candidates (top 3-5 file pairs + one-line rationale each)
## Stale / possibly wrong (memories that look outdated)
## Quarantine (any suspects noted in the curate output)
No tool calls. Output only the markdown." \
            --allowedTools "" > "$REPORT" 2>/dev/null

        ln -sfn "$OUTDIR" "${LOGROOT}/latest"
        printf '%s\n' "$REPORT" > "${LOGROOT}/.unread"
        echo "[$(date -Iseconds)] daytime claude -p done -> $REPORT"
    ) &
    exit 0
fi
echo "[$(date -Iseconds)] nighttime run (hour=${HOUR}) -> Ollama pipeline (store=$MEM_DIR)"

"$PYBIN" "${HOME}/.claude/memory_score.py" --json > "$SCORES" 2>/dev/null \
    || { echo "[$(date -Iseconds)] ERROR scoring; abort"; exit 0; }

{
    echo "# Memory maintenance report — ${TS}"
    echo
    echo "_Local maintenance pass. **Duplicate Finder** = semantic dedup/merge · **Injection Guard** = trust scoring + injection quarantine. Mutations stay human-gated._"
    echo
} > "$REPORT"

# 1. quarantine suspects (reversible) — fixation/security action
if [[ "$(python3 "$AI" --get light_pass.injection_guard.quarantine_suspects 2>/dev/null)" == "true" ]]; then
    SUSPECTS=$(python3 -c "import json;d=json.load(open('$SCORES'));print('\n'.join(m['name'] for m in d['memories'] if m['suspicion']))" 2>/dev/null)
    if [[ -n "$SUSPECTS" ]]; then
        echo "## ⚠ Quarantined suspects (possible injection — review before restoring)" >> "$REPORT"
        while IFS= read -r f; do
            [[ -z "$f" ]] && continue
            if [[ -f "${MEM_DIR}/${f}" ]]; then
                mv "${MEM_DIR}/${f}" "${QUAR}/${f}"
                if command -v memory_index_remove_line >/dev/null 2>&1; then
                    memory_index_remove_line "$INDEX" "$f" || true
                else
                    [[ -f "$INDEX" ]] && grep -vF "](${f})" "$INDEX" > "${INDEX}.tmp.$$" && mv "${INDEX}.tmp.$$" "$INDEX"
                fi
                echo "- \`${f}\` → moved to .quarantine/, de-indexed" >> "$REPORT"
                echo "[$(date -Iseconds)] quarantined $f"
                # Telegram RESTORE notification (zero-command: replaces the /memory-curate
                # suspect-review step). No-op without a token; auto-purges after probation.
                python3 "${HOME}/.claude/engram_telegram_gate.py" --notify-suspect "$f" >/dev/null 2>&1 || true
            fi
        done <<< "$SUSPECTS"
        command -v memory_push_index >/dev/null 2>&1 && memory_push_index "fixation: quarantine suspects" 2>/dev/null || true
        echo >> "$REPORT"
    fi
fi

# 2 + 3. light analysis: curation (embeddings) + fixation snapshot
"$PYBIN" "${HOME}/.claude/memory_light_curate.py" >> "$REPORT" 2>>"${LOGROOT}/cron.log" \
    || echo "_light pass errored — see cron.log_" >> "$REPORT"

# 3b. structural lint (report-only): Index↔section drift, duplicate headers,
# dangling wikilinks, frontmatter gaps. Advisory; never blocks.
{ echo; echo "## Structural lint"; echo; } >> "$REPORT"
python3 "${HOME}/.claude/memory_lint.py" >> "$REPORT" 2>>"${LOGROOT}/cron.log" \
    || echo "_lint errored — see cron.log_" >> "$REPORT"

# surface NOW so the report is usable even if distillation drafting is slow
ln -sfn "$OUTDIR" "${LOGROOT}/latest"
printf '%s\n' "$REPORT" > "${LOGROOT}/.unread"
echo "[$(date -Iseconds)] report surfaced -> $REPORT"

# 4. heavy, best-effort: draft cluster distillations — gated WEEKLY. Distillation is
# the LLM-costed stage, and survival-passes are the clock that mints "fixed"
# (survival>=3): distilling nightly would let a memory reach "fixed" in ~3 days,
# defeating "earns trust over time". Weekly makes survival>=3 mean ~3 weeks survived.
DISTILL_STAMP="${LOGROOT}/.last_distill"
DISTILL_EVERY_DAYS="$(python3 "$AI" --get light_pass.distill_every_days 2>/dev/null || echo 7)"
[[ "$DISTILL_EVERY_DAYS" =~ ^[0-9]+$ ]] || DISTILL_EVERY_DAYS=7
_last_distill=0; [[ -f "$DISTILL_STAMP" ]] && _last_distill="$(cat "$DISTILL_STAMP" 2>/dev/null || echo 0)"
_age_days=$(( ( $(date +%s) - _last_distill ) / 86400 ))
if [[ "$(python3 "$AI" --get light_pass.draft_distill 2>/dev/null)" == "true" ]] && (( _age_days >= DISTILL_EVERY_DAYS )); then
    { echo; echo "## Proposed distillations (drafts — NOT applied; weekly)"; echo; } >> "$REPORT"
    "$PYBIN" "${HOME}/.claude/memory_distill.py" >> "$REPORT" 2>>"${LOGROOT}/cron.log" \
        || echo "_distill drafting errored — see cron.log_" >> "$REPORT"
    date +%s > "$DISTILL_STAMP"
else
    echo "_distill skipped (weekly cadence: ${_age_days}d/${DISTILL_EVERY_DAYS}d since last)_" >> "$REPORT"
fi
echo "[$(date -Iseconds)] fixate/maintenance done -> $REPORT"

# NOTE: the unattended harvest->graduate->skill pipeline is NO LONGER run here — it
# is its own frequent daemon task (task_harvest, hourly) so encoding stays fresh while
# fixation runs nightly. See engram-daemon.py.
