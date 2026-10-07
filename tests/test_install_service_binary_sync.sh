#!/usr/bin/env bash
# A binary a systemd unit actually runs must be refreshed by install.sh.
#
# On an SELinux-enforcing host systemd cannot exec out of /root/.claude, so the
# API runs from a bin_t path like /usr/local/bin and the unit's ExecStart points
# there. install.sh only wrote $CLAUDE/rust, so every install left the service
# running the old binary while reporting success, and `systemctl restart` changed
# nothing. Found on a live host, after a "successful" install.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

CLAUDE="$TMP/claude"; REPO="$TMP/repo"; HOME="$TMP/home"
mkdir -p "$CLAUDE/rust" "$REPO/target/release" "$HOME/.config/systemd/user" "$TMP/etc"
printf 'NEW-BINARY\n' > "$REPO/target/release/engram-app"
chmod 755 "$REPO/target/release/engram-app"

# the out-of-tree copy a unit runs, holding stale content
mkdir -p "$TMP/usrlocal"
printf 'OLD-BINARY\n' > "$TMP/usrlocal/engram-app"; chmod 755 "$TMP/usrlocal/engram-app"
# ...and one the installer already owns, which must be left alone
printf 'NEW-BINARY\n' > "$CLAUDE/rust/engram-app"; chmod 755 "$CLAUDE/rust/engram-app"
# ...and a path referenced by a unit but absent: must NOT be created
ABSENT="$TMP/absent/engram-app"

cat > "$HOME/.config/systemd/user/engram-api.service" <<EOF
[Service]
ExecStart=$TMP/usrlocal/engram-app --config $CLAUDE/engram.yaml --bind 127.0.0.1:8787
EOF
cat > "$HOME/.config/systemd/user/engram-other.service" <<EOF
[Service]
ExecStart=$ABSENT --config $CLAUDE/engram.yaml
EOF
cat > "$HOME/.config/systemd/user/engram-managed.service" <<EOF
[Service]
ExecStart=$CLAUDE/rust/engram-app --config $CLAUDE/engram.yaml
EOF

# Extract the helper and run it against the fixture.
say(){ printf '%s\n' "$*"; }
eval "$(awk '/^sync_service_binaries\(\)\{/,/^\}/' "$ROOT/install.sh")"
export CLAUDE REPO HOME
sync_service_binaries >"$TMP/out" 2>&1 || { echo "FAIL: helper errored"; cat "$TMP/out"; exit 1; }

grep -q NEW-BINARY "$TMP/usrlocal/engram-app" \
  || { echo "FAIL: the binary the unit runs was not refreshed"; exit 1; }
grep -q "refreshed $TMP/usrlocal/engram-app" "$TMP/out" \
  || { echo "FAIL: refresh not reported"; cat "$TMP/out"; exit 1; }
[[ ! -e "$ABSENT" ]] \
  || { echo "FAIL: created a system-wide install nobody asked for"; exit 1; }
grep -q "restart" "$TMP/out" \
  || { echo "FAIL: did not tell the operator to restart"; cat "$TMP/out"; exit 1; }

# Idempotent: a second run has nothing to report about an already-current copy.
sync_service_binaries >"$TMP/out2" 2>&1
grep -q NEW-BINARY "$TMP/usrlocal/engram-app" || { echo "FAIL: second run broke it"; exit 1; }

echo "ok — install.sh refreshes the binary a unit runs, and creates none"
