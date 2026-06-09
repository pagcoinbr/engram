#!/usr/bin/env bash
# memory_pipeline.sh — the unattended memory pipeline (stages ①→⑤), all local.
#
#   ①  memory_harvest.py        transcripts  -> .staging/ (quarantined candidates)
#   ②  memory_stage_apply.py    .staging/    -> recall (clean, user-direct only)
#   ⑤  memory_skill_autoinstall.py  fixated memories -> installed skills
#
# Every stage is gated by memory_ai.yaml. `--apply` is passed to stages ②/⑤ but
# they NO-OP the mutation unless their own `enabled:` switch is true — so while the
# lights-out switches are off this run harvests + dry-runs the rest (safe to
# schedule immediately). Gated overall by local_enabled. Appends to the fixation
# log so the SessionStart nudge can surface it.
#
# Usage:
#   memory_pipeline.sh                 # full run (harvest + apply-or-dryrun + autoinstall)
#   memory_pipeline.sh --harvest-only  # stage ① only
#   memory_pipeline.sh --dry           # never pass --apply to any stage

set -uo pipefail
source "${HOME}/.claude/memory_lib.sh" 2>/dev/null || true
AI="${HOME}/.claude/memory_ai.py"
LOGROOT="${HOME}/.claude/logs/fixation"
PIPELOG="${HOME}/.claude/logs/pipeline.log"
mkdir -p "$LOGROOT" "$(dirname "$PIPELOG")"

log() { echo "[$(date -Iseconds)] $*" >> "$PIPELOG"; }

# Master local switch.
if [[ "$(python3 "$AI" --get local_enabled 2>/dev/null)" != "true" ]]; then
    log "local_enabled=false — pipeline skipped"; exit 0
fi

HARVEST_ONLY=0; APPLY="--apply"
for a in "$@"; do
    case "$a" in
        --harvest-only) HARVEST_ONLY=1 ;;
        --dry)          APPLY="" ;;
    esac
done

log "pipeline start (apply='${APPLY:-dry}')"

# ① Harvest new transcript turns into .staging/ (local qwen3-coder:30b).
log "stage ① harvest"
python3 "${HOME}/.claude/memory_harvest.py" >>"$PIPELOG" 2>&1 \
    || log "stage ① harvest errored (see pipeline.log)"

if (( HARVEST_ONLY )); then
    log "harvest-only — done"; exit 0
fi

# ②/④ Graduate clean staged candidates (or dry-run if auto_graduate.enabled=false).
log "stage ②/④ graduate"
python3 "${HOME}/.claude/memory_stage_apply.py" $APPLY >>"$PIPELOG" 2>&1 \
    || log "stage ②/④ graduate errored"

# ⑤ Guarded skill auto-install (or dry-run if skill_autoinstall.enabled=false).
log "stage ⑤ skill auto-install"
python3 "${HOME}/.claude/memory_skill_autoinstall.py" $APPLY >>"$PIPELOG" 2>&1 \
    || log "stage ⑤ skill auto-install errored"

log "pipeline done"
