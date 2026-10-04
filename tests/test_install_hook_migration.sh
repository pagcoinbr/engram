#!/usr/bin/env bash
# Regression: exactly ONE recall hook may be registered in settings.json.
#
# merge_hook only ever appended. An install that had registered the Python recall
# hook and later gained the Rust binary ended up with BOTH UserPromptSubmit
# entries, each keeping its own per-session dedup state — so every prompt paid for
# two recalls and could be injected with the same memories twice.
#
# This drives the real jq filters out of install.sh rather than a copy, so the test
# cannot pass against a stale duplicate of the logic.
set -euo pipefail

command -v jq >/dev/null || { echo "skip — jq not installed"; exit 0; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

CLAUDE="$TMP/.claude"
mkdir -p "$CLAUDE/hooks" "$CLAUDE/rust"
SETTINGS="$CLAUDE/settings.json"
RUST_HOOK="$CLAUDE/rust/engram-recall-hook"
PY_HOOK="$CLAUDE/hooks/memory-recall-inject.py"

# Extract merge_hook and replace_hook from install.sh so the test exercises the
# shipped implementation.
eval "$(awk '/^  merge_hook\(\)\{/,/^  }$/' "$ROOT/install.sh" | sed 's/^  //')"
eval "$(awk '/^  replace_hook\(\)\{/,/^  }$/' "$ROOT/install.sh" | sed 's/^  //')"
type replace_hook >/dev/null 2>&1 || { echo "FAIL: replace_hook not found in install.sh"; exit 1; }

count_hooks() { jq --arg e UserPromptSubmit '[.hooks[$e][]?|.hooks[]?|.command]|length' "$SETTINGS"; }
commands()    { jq -r --arg e UserPromptSubmit '[.hooks[$e][]?|.hooks[]?|.command]|sort|join(",")' "$SETTINGS"; }

# ---- 1. upgrade: a Python hook is already registered, the Rust binary appears --
echo '{}' > "$SETTINGS"
merge_hook UserPromptSubmit "$PY_HOOK"
[[ "$(count_hooks)" == 1 ]] || { echo "FAIL: setup did not register the Python hook"; exit 1; }

replace_hook UserPromptSubmit "$RUST_HOOK" "$PY_HOOK"
got="$(count_hooks)"
[[ "$got" == 1 ]] || { echo "FAIL: upgrade left $got recall hooks registered, not 1"; jq . "$SETTINGS"; exit 1; }
[[ "$(commands)" == "$RUST_HOOK" ]] || { echo "FAIL: wrong hook survived: $(commands)"; exit 1; }
echo "ok — upgrading to the Rust hook removes the Python one"

# ---- 2. rollback: the Rust binary goes away again -----------------------------
replace_hook UserPromptSubmit "$PY_HOOK" "$RUST_HOOK"
[[ "$(count_hooks)" == 1 && "$(commands)" == "$PY_HOOK" ]] || {
  echo "FAIL: rollback did not restore a single Python hook: $(commands)"; exit 1; }
echo "ok — rolling back removes the Rust hook"

# ---- 3. idempotent: re-running the installer must not duplicate ---------------
replace_hook UserPromptSubmit "$PY_HOOK" "$RUST_HOOK"
replace_hook UserPromptSubmit "$PY_HOOK" "$RUST_HOOK"
[[ "$(count_hooks)" == 1 ]] || { echo "FAIL: re-install duplicated the hook"; exit 1; }
echo "ok — re-running the installer is idempotent"

# ---- 4. the pathological state this bug actually produced ---------------------
# Both hooks registered. One pass must leave exactly one.
echo '{}' > "$SETTINGS"
merge_hook UserPromptSubmit "$PY_HOOK"
merge_hook UserPromptSubmit "$RUST_HOOK"
[[ "$(count_hooks)" == 2 ]] || { echo "FAIL: could not reproduce the double-hook state"; exit 1; }
replace_hook UserPromptSubmit "$RUST_HOOK" "$PY_HOOK"
[[ "$(count_hooks)" == 1 && "$(commands)" == "$RUST_HOOK" ]] || {
  echo "FAIL: an existing double registration was not repaired: $(commands)"; jq . "$SETTINGS"; exit 1; }
echo "ok — an already-broken install is repaired to a single hook"

# ---- 5. unrelated hooks and events must be untouched --------------------------
echo '{}' > "$SETTINGS"
merge_hook SessionStart "$CLAUDE/memory_curate_check.sh"
merge_hook Stop "$CLAUDE/memory_agent.sh"
merge_hook UserPromptSubmit "$PY_HOOK"
merge_hook UserPromptSubmit "$CLAUDE/someone-elses-hook.sh"
replace_hook UserPromptSubmit "$RUST_HOOK" "$PY_HOOK"
jq -e --arg c "$CLAUDE/memory_curate_check.sh" '[.hooks.SessionStart[]?|.hooks[]?|.command]|index($c)' "$SETTINGS" >/dev/null \
  || { echo "FAIL: a SessionStart hook was collateral damage"; exit 1; }
jq -e --arg c "$CLAUDE/memory_agent.sh" '[.hooks.Stop[]?|.hooks[]?|.command]|index($c)' "$SETTINGS" >/dev/null \
  || { echo "FAIL: a Stop hook was collateral damage"; exit 1; }
jq -e --arg c "$CLAUDE/someone-elses-hook.sh" '[.hooks.UserPromptSubmit[]?|.hooks[]?|.command]|index($c)' "$SETTINGS" >/dev/null \
  || { echo "FAIL: a third-party UserPromptSubmit hook was removed"; exit 1; }
echo "ok — only the named obsolete command is removed"

echo "ok — install hook migration"
