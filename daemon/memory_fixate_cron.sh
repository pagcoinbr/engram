#!/usr/bin/env bash
# memory_fixate_cron.sh — twice-daily LIGHT memory maintenance (systemd timer).
#
# Gated by memory_ai.yaml `local_enabled`. All work is local (Ollama on the LAN):
#   1. score + auto-QUARANTINE suspect/injection memories (reversible)
#   2. CURATION: semantic near-duplicate / cluster detection (embeddings expert)
#   3. FIXATION: trust-signal snapshot
#   4. surface the report, THEN (best-effort, bounded) draft cluster distillations
#
# It NEVER merges or deletes memories unattended — those stay human-gated via
# /memory-curate apply and /memory-fixate apply. Suspects are quarantined (moved
# out of recall), never deleted.

set -uo pipefail
source "${HOME}/.claude/memory_lib.sh" 2>/dev/null || true
AI="${HOME}/.claude/memory_ai.py"
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
echo "[$(date -Iseconds)] maintenance start (store=$MEM_DIR)"

python3 "${HOME}/.claude/memory_score.py" --json > "$SCORES" 2>/dev/null \
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
            fi
        done <<< "$SUSPECTS"
        command -v memory_push_index >/dev/null 2>&1 && memory_push_index "fixation: quarantine suspects" 2>/dev/null || true
        echo >> "$REPORT"
    fi
fi

# 2 + 3. light analysis: curation (embeddings) + fixation snapshot
python3 "${HOME}/.claude/memory_light_curate.py" >> "$REPORT" 2>>"${LOGROOT}/cron.log" \
    || echo "_light pass errored — see cron.log_" >> "$REPORT"

# surface NOW so the report is usable even if distillation drafting is slow
ln -sfn "$OUTDIR" "${LOGROOT}/latest"
printf '%s\n' "$REPORT" > "${LOGROOT}/.unread"
echo "[$(date -Iseconds)] report surfaced -> $REPORT"

# 4. heavy, best-effort, bounded: draft cluster distillations (appended)
if [[ "$(python3 "$AI" --get light_pass.draft_distill 2>/dev/null)" == "true" ]]; then
    { echo; echo "## Proposed distillations (drafts — NOT applied; run /memory-fixate apply to act)"; echo; } >> "$REPORT"
    python3 "${HOME}/.claude/memory_distill.py" >> "$REPORT" 2>>"${LOGROOT}/cron.log" \
        || echo "_distill drafting errored — see cron.log_" >> "$REPORT"
fi
echo "[$(date -Iseconds)] maintenance done -> $REPORT"

# 5. Unattended PIPELINE (stages ①→⑤): harvest transcripts -> stage -> graduate
# -> guarded skill auto-install. Self-gated by memory_ai.yaml (auto_graduate /
# skill_autoinstall default OFF, so this harvests + dry-runs until enabled).
# Appended to its own pipeline.log; a one-line pointer goes into the report.
{
    echo
    echo "## Unattended pipeline (harvest → graduate → skill auto-install)"
    echo "_See ~/.claude/logs/pipeline.log for stage detail. Lights-out switches:"
    echo "auto_graduate.enabled=$(python3 "$AI" --get auto_graduate.enabled 2>/dev/null) · "
    echo "skill_autoinstall.enabled=$(python3 "$AI" --get skill_autoinstall.enabled 2>/dev/null)._"
} >> "$REPORT"
bash "${HOME}/.claude/memory_pipeline.sh" >>"${LOGROOT}/cron.log" 2>&1 \
    || echo "_pipeline errored — see pipeline.log_" >> "$REPORT"
echo "[$(date -Iseconds)] pipeline done"
