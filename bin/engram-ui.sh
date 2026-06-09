#!/usr/bin/env bash
# engram-ui.sh — launch the local engram GUI (binds 127.0.0.1 only).
#   engram-ui.sh            # http://127.0.0.1:8765
#   ENGRAM_UI_PORT=9000 engram-ui.sh
set -eo pipefail
CLAUDE="${ENGRAM_CLAUDE_HOME:-$HOME/.claude}"
export ENGRAM_BIN="${ENGRAM_BIN:-$CLAUDE}"
export ENGRAM_GRAPH="${ENGRAM_GRAPH:-$CLAUDE/graph}"

# UI assets: installed copy, else this repo's ui/
REPO_UI="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." 2>/dev/null && pwd)/ui"
export ENGRAM_UI_DIR="${ENGRAM_UI_DIR:-$CLAUDE/ui}"
[[ -f "$ENGRAM_UI_DIR/index.html" ]] || export ENGRAM_UI_DIR="$REPO_UI"

# Prefer the graph venv python (it has graphiti/neo4j; we add fastapi there too).
PY="$CLAUDE/graph/venv/bin/python"
[[ -x "$PY" ]] || PY="$(command -v python3)"
if ! "$PY" -c "import fastapi, uvicorn" 2>/dev/null; then
  echo "[engram] installing fastapi + uvicorn for the GUI..."
  "$PY" -m pip install -q fastapi uvicorn 2>/dev/null || "$PY" -m pip install --user -q fastapi uvicorn
fi
exec "$PY" "$ENGRAM_BIN/engram_api.py"
