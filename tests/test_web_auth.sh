#!/usr/bin/env bash
# Guards the web-auth security boundary (install.sh --web-auth).
#
# It cannot stand up a live gateway, so it asserts the invariants that make the
# feature correct and safe, against the real install.sh:
#   1. the engram-router patch is idempotent, attaches engram-auth, and REFUSES
#      to lossily rewrite a commented gateway file when ruamel is absent;
#   2. the middleware uses INLINE users (not usersFile) — the gateway mounts
#      individual files, so a usersFile would be invisible in the container;
#   3. "web only": no auth is bolted onto the loopback engram-app / API / CLI.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL="$ROOT/install.sh"
fail(){ echo "FAIL test_web_auth — $1"; exit 1; }

command -v python3 >/dev/null 2>&1 || { echo "SKIP test_web_auth — no python3"; exit 0; }

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

# 1. Extract the embedded tls-patch python exactly as install.sh runs it.
awk '/python3 - "\$tls" <<.PY./{f=1;next} f&&/^PY$/{exit} f{print}' "$INSTALL" > "$WORK/patch.py"
[ -s "$WORK/patch.py" ] || fail "could not extract the tls patch from install.sh"
python3 -c "compile(open('$WORK/patch.py').read(),'p','exec')" || fail "extracted tls patch does not compile"

# a minimal gateway tls.yml with an engram router and a sibling to protect
cat > "$WORK/tls.yml" <<'YAML'
http:
  routers:
    whonix:
      rule: "Host(`whonix.home.arpa`)"
      middlewares: [whonix-auth]
      service: whonix-novnc
    engram:
      rule: "Host(`engram.home.arpa`)"
      service: engram-web
YAML

python3 "$WORK/patch.py" "$WORK/tls.yml" || fail "patch run failed"
python3 - "$WORK/tls.yml" <<'PY' || exit 1
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
mw = d["http"]["routers"]["engram"].get("middlewares") or []
assert "engram-auth" in mw, f"engram-auth not attached: {mw}"
# the other router must be untouched
assert d["http"]["routers"]["whonix"]["middlewares"] == ["whonix-auth"], "sibling router changed"
PY
[ $? -eq 0 ] || fail "engram router not patched correctly"

# idempotent: a second run leaves it byte-for-byte
before="$(md5sum "$WORK/tls.yml" | cut -d' ' -f1)"
python3 "$WORK/patch.py" "$WORK/tls.yml" || fail "second patch run failed"
after="$(md5sum "$WORK/tls.yml" | cut -d' ' -f1)"
[ "$before" = "$after" ] || fail "patch is not idempotent"

# a commented gateway file must NOT be silently rewritten when ruamel is absent
if ! python3 -c "import ruamel.yaml" 2>/dev/null; then
  printf '# operator notes\n' > "$WORK/commented.yml"; cat "$WORK/tls.yml" >> "$WORK/commented.yml"
  pre="$(md5sum "$WORK/commented.yml" | cut -d' ' -f1)"
  python3 "$WORK/patch.py" "$WORK/commented.yml" 2>/dev/null && fail "patched a commented file lossily (should refuse)"
  post="$(md5sum "$WORK/commented.yml" | cut -d' ' -f1)"
  [ "$pre" = "$post" ] || fail "refused but still modified the commented file"
fi

# 2. the generated middleware must use inline users, not a usersFile key.
#    Match an actual YAML key (indented `usersFile:`), not the word in a comment.
grep -Eq '^\s+usersFile:' "$INSTALL" && fail "install.sh emits a usersFile: key (not mounted in the gateway container) — use inline users"
grep -Eq '^\s+users:\s*$' "$INSTALL" || fail "install.sh web-auth middleware does not emit an inline 'users:' key"

# 3. web-only: the Rust API/app must carry NO request auth (no Authorization/401
#    gate). engram's loopback surface stays open by design.
if grep -rnE '\b(WWW-Authenticate|StatusCode::UNAUTHORIZED|401 Unauthorized)\b' "$ROOT/crates/engram-app/src/" 2>/dev/null | grep -v test; then
  fail "engram-app grew request auth — web auth must live at the gateway only"
fi

echo "PASS test_web_auth"
exit 0
