#!/usr/bin/env bash
# uninstall.sh — remove engram from ~/.claude. KEEPS your memory store by default.
#   --purge   also remove the memory store, engram.yaml/engram.env, the graph venv + .env,
#             and the vector venv + Qdrant storage
set -eo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLAUDE="${ENGRAM_CLAUDE_HOME:-$HOME/.claude}"
SETTINGS="$CLAUDE/settings.json"
PURGE=0; [[ "${1:-}" == "--purge" ]] && PURGE=1
say(){ printf '\033[1;36m[engram]\033[0m %s\n' "$*"; }

# Stop running services FIRST — before removing any engine files — so an in-flight
# nightly apply can't be killed mid-plan after its helper scripts are deleted.
systemctl --user disable --now engram.timer memory-nightly-apply.timer >/dev/null 2>&1 || true
systemctl --user stop memory-nightly-apply.service engram.service >/dev/null 2>&1 || true

# engine files (by the names shipped in the repo)
[[ -d "$REPO/bin" ]] && for f in "$REPO"/bin/*; do rm -f "$CLAUDE/$(basename "$f")"; done
[[ -d "$REPO/commands" ]] && for f in "$REPO"/commands/*; do rm -f "$CLAUDE/commands/$(basename "$f")"; done
rm -f "$CLAUDE/engram-daemon.py"
[[ -d "$REPO/graph" ]] && for f in "$REPO"/graph/*.py "$REPO"/graph/*.md; do rm -f "$CLAUDE/graph/$(basename "$f")"; done
rm -f "$CLAUDE/graph/docker-compose.yml"
[[ -d "$REPO/vector" ]] && for f in "$REPO"/vector/*.py; do rm -f "$CLAUDE/vector/$(basename "$f")"; done
rm -f "$CLAUDE/vector/docker-compose.yml" "$CLAUDE/vector/sync_state.json"
say "removed engine files"

# hooks (drop our commands; prune now-empty groups)
if command -v jq >/dev/null && [[ -f "$SETTINGS" ]]; then
  if jq '.hooks //= {} | .hooks |= (to_entries
        | map(.value |= (map(.hooks |= map(select((.command // "")
              | test("(memory_curate_check|memory_agent|memory_session_curate|codex-availability-warn)\\.sh")|not)))
            | map(select((.hooks|length) > 0))))
        | from_entries)' "$SETTINGS" > "$SETTINGS.tmp"; then
    mv "$SETTINGS.tmp" "$SETTINGS"; say "removed hooks from settings.json"
  else rm -f "$SETTINGS.tmp"; fi
fi

# MCP servers
command -v claude >/dev/null && claude mcp remove engram-graph >/dev/null 2>&1 && say "removed engram-graph MCP" || true
command -v claude >/dev/null && claude mcp remove engram-vector >/dev/null 2>&1 && say "removed engram-vector MCP" || true

# daemon unit files (services already stopped at the top)
rm -f "$HOME/.config/systemd/user/engram.service" "$HOME/.config/systemd/user/engram.timer" \
      "$HOME/.config/systemd/user/memory-nightly-apply.service" "$HOME/.config/systemd/user/memory-nightly-apply.timer" \
      "$HOME/.config/engram/daemon.env"
systemctl --user daemon-reload >/dev/null 2>&1 || true
say "removed daemon units"

if [[ "$PURGE" == 1 ]]; then
  SLUG="${CLAUDE_MEMORY_SLUG:-$(printf '%s' "$HOME" | sed 's|/|-|g')}"
  rm -rf "$CLAUDE/graph/venv" "$CLAUDE/graph/.env" "$CLAUDE/vector/venv" "$CLAUDE/vector/qdrant_storage" \
         "$CLAUDE/engram.yaml" "$CLAUDE/engram.env" "$CLAUDE/projects/$SLUG/memory"
  say "PURGED graph + vector venvs + config + memory store"
else
  say "kept your memory store + engram.yaml (use --purge to remove them too)"
fi
say "done"
