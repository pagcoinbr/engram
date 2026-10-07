#!/usr/bin/env bash
# test_update.sh — update.sh must be safe by default and restore local overrides.
#
# Covered:
#   1. dry-run (the default) changes nothing: HEAD, files and the install all untouched;
#   2. a dirty tree, a non-default branch and a diverged branch are refused;
#   3. --apply fast-forwards, runs the installer with the detected daemon mode, and
#      copies ~/.claude/engram-local-overrides/* back over what the install wrote.
# The real install.sh is replaced (ENGRAM_INSTALL_CMD) by a stub that records its
# arguments and overwrites the overridden file, exactly as a reinstall does.
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
fails=0
ok()  { printf '  ✅ %s\n' "$1"; }
bad() { printf '  ❌ %s\n' "$1"; fails=$((fails + 1)); }

T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
git init -q --bare -b main "$T/origin.git"
git clone -q "$T/origin.git" "$T/seed" 2>/dev/null
cp "$REPO/update.sh" "$T/seed/" && touch "$T/seed/install.sh" && chmod +x "$T/seed/install.sh"
git -C "$T/seed" add -A && git -C "$T/seed" -c user.email=t@t -c user.name=t commit -qm base
git -C "$T/seed" push -q origin main
git clone -q "$T/origin.git" "$T/work"
echo new > "$T/seed/NEW" && git -C "$T/seed" add NEW \
  && git -C "$T/seed" -c user.email=t@t -c user.name=t commit -qm "new upstream commit" && git -C "$T/seed" push -q origin main

C="$T/home/.claude"; mkdir -p "$C/commands" "$C/engram-local-overrides/commands"
echo '{}' > "$C/settings.json"
echo "my customised command" > "$C/engram-local-overrides/commands/custom.md"
cat > "$T/install-stub" <<EOF
#!/usr/bin/env bash
echo "\$*" > "$T/install-args"
echo "repo copy" > "$C/commands/custom.md"
EOF
chmod +x "$T/install-stub"
# No user systemd bus: the sandbox must never see (let alone restart) the real
# engram-atlas / engram-api services of whoever runs the test.
run() { (cd "$T/work" && HOME="$T/home" ENGRAM_CLAUDE_HOME="$C" ENGRAM_INSTALL_CMD="$T/install-stub" \
        XDG_RUNTIME_DIR="$T/run" DBUS_SESSION_BUS_ADDRESS="unix:path=$T/run/none" \
        ./update.sh "$@" >"$T/out" 2>&1); }

# ── 1. dry-run changes nothing ───────────────────────────────────────────────
before="$(git -C "$T/work" rev-parse HEAD)"
run && grep -q "new upstream commit" "$T/out" \
  && ok "dry-run lists the incoming commit" || { bad "dry-run did not list the commit"; cat "$T/out"; }
[[ "$(git -C "$T/work" rev-parse HEAD)" == "$before" && ! -e "$T/work/NEW" && ! -e "$T/install-args" ]] \
  && ok "dry-run moved nothing and did not run the installer" || bad "dry-run changed state"

# ── 2. refusals ──────────────────────────────────────────────────────────────
echo dirty >> "$T/work/update.sh"
run --apply; rc=$?
((rc != 0)) && grep -q "uncommitted changes" "$T/out" && ok "refuses a dirty tree" || bad "dirty tree not refused"
git -C "$T/work" checkout -q -- update.sh
git -C "$T/work" switch -q -c feature
run --apply; rc=$?
((rc != 0)) && grep -q "default branch only" "$T/out" && ok "refuses a non-default branch" || bad "branch not refused"
git -C "$T/work" switch -q main
echo local > "$T/work/LOCAL" && git -C "$T/work" add LOCAL \
  && git -C "$T/work" -c user.email=t@t -c user.name=t commit -qm local
run --apply; rc=$?
((rc != 0)) && grep -q "diverged" "$T/out" && ok "refuses a diverged branch" || bad "diverged branch not refused"
git -C "$T/work" reset -q --hard "$before"

# ── 3. apply: fast-forward, detected daemon mode, overrides restored ─────────
run --apply
[[ -e "$T/work/NEW" ]] && ok "--apply fast-forwarded to origin" || bad "--apply did not pull"
grep -q -- "--yes --daemon none" "$T/install-args" 2>/dev/null \
  && ok "installer ran with the detected daemon mode (none)" || bad "installer args: $(cat "$T/install-args" 2>/dev/null)"
[[ "$(cat "$C/commands/custom.md")" == "my customised command" ]] \
  && ok "local override restored over the reinstalled copy" || bad "override lost: $(cat "$C/commands/custom.md")"
grep -q "Atlas" "$T/out" && bad "sandbox saw a real Atlas service" || ok "sandbox is isolated from the user's services"
grep -q "injected NOTHING\|no engram recall hook" "$T/out" \
  && ok "verify step reports the missing hook instead of passing" || bad "verify did not flag a missing hook"

((fails == 0)) && echo "update.sh: all checks passed" || { echo "update.sh: $fails check(s) failed"; exit 1; }
