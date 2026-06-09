#!/usr/bin/env bash
# install_fixate_timer.sh — install the TWICE-DAILY memory-maintenance systemd
# USER timer. Idempotent. Times are read from memory_ai.yaml (schedule.times).
# Run as your normal user (no sudo for the install itself).
set -euo pipefail

UNIT_DIR="${HOME}/.config/systemd/user"
mkdir -p "$UNIT_DIR"

# Build OnCalendar lines from config (fallback to 03:30 & 15:30).
TIMES_JSON="$(python3 "${HOME}/.claude/memory_ai.py" --get schedule.times 2>/dev/null || echo '["03:30","15:30"]')"
ONCAL="$(echo "$TIMES_JSON" | jq -r '.[] | "OnCalendar=*-*-* " + . + ":00"' 2>/dev/null)"
[[ -z "$ONCAL" ]] && ONCAL=$'OnCalendar=*-*-* 03:30:00\nOnCalendar=*-*-* 15:30:00'

cat > "${UNIT_DIR}/memory-fixate.service" <<EOF
[Unit]
Description=Local memory maintenance (curation + fixation, MoE via Ollama)

[Service]
Type=oneshot
ExecStart=${HOME}/.claude/memory_fixate_cron.sh
EOF

cat > "${UNIT_DIR}/memory-fixate.timer" <<EOF
[Unit]
Description=Run local memory maintenance twice daily

[Timer]
${ONCAL}
Persistent=true

[Install]
WantedBy=timers.target
EOF

echo "Wrote units to ${UNIT_DIR}:"
echo "$ONCAL" | sed 's/^/  /'

if systemctl --user daemon-reload 2>/dev/null && \
   systemctl --user enable --now memory-fixate.timer 2>/dev/null; then
    echo "Timer enabled. Schedule:"
    systemctl --user list-timers memory-fixate.timer --no-pager 2>/dev/null || true
else
    echo
    echo "NOTE: 'systemctl --user' couldn't reach your user systemd bus from here."
    echo "Run these IN YOUR OWN shell (prefix with '!' inside Claude Code):"
    echo "    systemctl --user daemon-reload"
    echo "    systemctl --user enable --now memory-fixate.timer"
fi

echo
echo "For the timer to fire even when you are NOT logged in (recommended), enable lingering (sudo):"
echo "    sudo loginctl enable-linger ${USER}"
echo
echo "Toggle local processing any time:  edit local_enabled in ~/.claude/memory_ai.yaml"
echo "Manual run:  ~/.claude/memory_fixate_cron.sh   (report: ~/.claude/logs/fixation/latest/REPORT.md)"
