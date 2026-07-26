#!/usr/bin/env bash
# test_install_hermes.sh — install.sh must also expose the engram MCP servers to
# hermes (an MCP client driving a LOCAL Ollama model) when hermes is installed.
#
# Regressions covered:
#   1. The slug must travel in the entry's OWN --env. hermes sanitizes the child
#      env down to PATH/HOME/USER/LANG/LC_ALL/TERM/SHELL/TMPDIR/XDG_*, so an
#      exported CLAUDE_MEMORY_SLUG never reaches the server and recall silently
#      targets the wrong store.
#   2. `hermes mcp add --args` is argparse.REMAINDER — anything after it is
#      swallowed, so --args MUST be the last flag on the line.
#   3. Registration must be idempotent: install.sh is also the UPDATER.
#   4. No hermes on PATH (and --no-hermes) must be a silent no-op, never a failure.
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
fails=0
ok()  { printf '  ✅ %s\n' "$1"; }
bad() { printf '  ❌ %s\n' "$1"; fails=$((fails + 1)); }

T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT

# fake hermes: records argv, and reports "already registered" only once primed
mkdir -p "$T/bin"
cat > "$T/bin/hermes" <<'EOF'
#!/usr/bin/env bash
if [[ "$1 $2" == "mcp list" ]]; then [[ -f "$FAKE_HERMES_STATE" ]] && cat "$FAKE_HERMES_STATE"; exit 0; fi
if [[ "$1 $2" == "mcp add" ]]; then printf '%s\n' "$*" >> "$FAKE_HERMES_CALLS"; printf '%s\n' "$3" >> "$FAKE_HERMES_STATE"; exit 0; fi
exit 0
EOF
chmod +x "$T/bin/hermes"

# extract hermes_register from install.sh and drive it directly — running the whole
# installer would build real venvs (minutes, network).
load_fn() { eval "$(sed -n '/^hermes_register()/,/^}/p' "$REPO/install.sh")"; }
say()  { :; }
warn() { printf 'WARN %s\n' "$*" >> "$WARNS"; }

# HOME is redirected at the sandbox so a stray call to a REAL hermes on this box
# can never read or mutate the operator's ~/.hermes/config.yaml.
export HOME="$T"
export FAKE_HERMES_CALLS="$T/calls" FAKE_HERMES_STATE="$T/state" WARNS="$T/warns"
: > "$FAKE_HERMES_CALLS"; : > "$WARNS"
PATH="$T/bin:$PATH"
SLUG="-pinned-store"
PYBIN="$T/python"; touch "$PYBIN"; chmod +x "$PYBIN"
load_fn

# ── 1. auto-registers when hermes is present ──────────────────────────────────
WANT_HERMES="auto"
hermes_register engram-vector "$PYBIN" "/srv/vector_mcp_server.py"
CALL="$(cat "$FAKE_HERMES_CALLS")"
[[ -n "$CALL" ]] && ok "auto: registered with hermes" || bad "auto: no hermes mcp add call"
grep -q -- "--env CLAUDE_MEMORY_SLUG=-pinned-store" <<<"$CALL" \
    && ok "slug passed via per-server --env (survives hermes' env sanitizer)" \
    || bad "slug NOT in --env — recall would hit the wrong store: $CALL"
[[ "$CALL" == *"--args /srv/vector_mcp_server.py" ]] \
    && ok "--args is last (argparse.REMAINDER safe)" \
    || bad "--args is not the final flag — hermes would swallow the rest: $CALL"

# ── 2. idempotent: a second run must not re-add ───────────────────────────────
BEFORE="$(wc -l < "$FAKE_HERMES_CALLS")"
hermes_register engram-vector "$PYBIN" "/srv/vector_mcp_server.py"
[[ "$(wc -l < "$FAKE_HERMES_CALLS")" == "$BEFORE" ]] \
    && ok "idempotent: already-registered server is not re-added" \
    || bad "re-registered an existing server (install.sh is also the updater)"

# ── 3. --no-hermes opts out even when hermes is installed ────────────────────
WANT_HERMES="no"
hermes_register engram-graph "$PYBIN" "/srv/mg_mcp_server.py"
grep -q "engram-graph" "$FAKE_HERMES_CALLS" \
    && bad "--no-hermes still registered" \
    || ok "--no-hermes skips registration"

# ── 4. hermes absent: silent no-op on auto, warning on explicit --hermes ──────
# A minimal PATH, not "strip the fake": a REAL hermes elsewhere on the operator's
# PATH (~/.local/bin) would otherwise answer these probes and hide the regression.
WANT_HERMES="auto"; PATH="/usr/bin:/bin"
hermes_register engram-graph "$PYBIN" "/srv/mg_mcp_server.py"
rc=$?
[[ $rc -eq 0 ]] && ok "auto + no hermes: returns 0 (set -e safe)" || bad "auto + no hermes returned $rc — would abort install.sh"
[[ -s "$WARNS" ]] && bad "auto + no hermes should stay silent: $(cat "$WARNS")" || ok "auto + no hermes: silent"
WANT_HERMES="yes"
hermes_register engram-graph "$PYBIN" "/srv/mg_mcp_server.py"
grep -q "hermes not found" "$WARNS" \
    && ok "explicit --hermes warns when hermes is missing" \
    || bad "explicit --hermes gave no warning"

# ── 5. the installer still parses + advertises the flags ─────────────────────
bash -n "$REPO/install.sh" && ok "install.sh syntax OK" || bad "install.sh syntax error"
grep -q -- "--no-hermes) WANT_HERMES=\"no\"" "$REPO/install.sh" \
    && ok "--no-hermes flag parsed" || bad "--no-hermes not in the arg parser"
"$REPO/install.sh" --help | grep -q -- "--hermes" \
    && ok "--hermes documented in --help" || bad "--hermes missing from --help output"

# ── 6. the graph venv must install mcp (mg_mcp_server.py imports it) ─────────
grep -q '"mcp\[cli\]" graphiti-core' "$REPO/install.sh" \
    && ok "graph venv build includes mcp" \
    || bad "graph venv still built without mcp — engram-graph dies at import"

echo "── test_install_hermes: $([[ $fails -eq 0 ]] && echo PASS || echo "FAIL ($fails)") ──"
exit $((fails > 0))
