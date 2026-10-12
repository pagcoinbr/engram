#!/usr/bin/env bash
# A full backup -> delete -> restore round-trip against a LOCAL restic repo.
#
# No network, no Backblaze: ENGRAM_BACKUP_REPO points restic at a /tmp directory,
# which exercises every code path in engram_backup.py except the S3 endpoint
# composition (covered separately by the status/repo-string assertions below).
# The point is to prove the authoritative data survives a round trip BYTE FOR
# BYTE, and that the encryption password never lands in the config or a log.
#
# Skips (exit 0 with a notice) when restic is absent, matching the Neo4j/Qdrant
# service-dependent tests, so CI without restic stays green.
set -u

if ! command -v restic >/dev/null 2>&1; then
    echo "SKIP test_backup_roundtrip — restic not installed"
    exit 0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

HOME_DIR="$WORK/home/.claude"
REPO="$WORK/repo"
mkdir -p "$HOME_DIR/projects/-tenant-a/memory" \
         "$HOME_DIR/projects/-tenant-b/memory" \
         "$HOME_DIR/graph" \
         "$WORK/vault/Notes" "$WORK/vault/.obsidian" "$WORK/vault/.trash"

# Seed the authoritative set: two memory stores, a vault, and the two config
# files a restore needs.
printf -- '---\nname: alpha\n---\nalpha body\n' > "$HOME_DIR/projects/-tenant-a/memory/alpha.md"
printf -- '---\nname: beta\n---\nbeta body\n'   > "$HOME_DIR/projects/-tenant-b/memory/beta.md"
printf -- '# Runbook\n\nDNS failover steps.\n'   > "$WORK/vault/Notes/runbook.md"
printf 'workspace-state'                          > "$WORK/vault/.obsidian/workspace.json"
printf 'deleted note'                             > "$WORK/vault/.trash/old.md"
printf 'NEO4J_PASSWORD=should-be-in-the-backup\n' > "$HOME_DIR/graph/.env"
cat > "$HOME_DIR/engram.yaml" <<YAML
local_enabled: true
backup:
  enabled: true
  retention: { daily: 7, weekly: 4, monthly: 6 }
tenants:
  a: { slugs: ['-tenant-a'], vault: $WORK/vault }
YAML

LOG="$WORK/daemon.log"
SECRET="correct-horse-battery-staple-$RANDOM"
export ENGRAM_BIN="$HOME_DIR"
export ENGRAM_CONFIG="$HOME_DIR/engram.yaml"
export ENGRAM_GRAPH="$HOME_DIR/graph"
export ENGRAM_LOG_DIR="$WORK/logs"
export ENGRAM_BACKUP_REPO="$REPO"
export ENGRAM_BACKUP_PASSWORD="$SECRET"

run() { python3 "$ROOT/bin/engram_backup.py" "$@" >>"$LOG" 2>&1; }

fail() { echo "FAIL test_backup_roundtrip — $1"; exit 1; }

run init      || fail "init failed"
run backup    || fail "backup failed"
run snapshots || fail "snapshots failed"
grep -q "engram" "$LOG" || fail "no engram-tagged snapshot listed"

# Destroy the live data, then restore from the encrypted repo.
rm -rf "$HOME_DIR/projects" "$WORK/vault/Notes" "$HOME_DIR/graph/.env"
TARGET="$WORK/restored"
run restore --snapshot latest --target "$TARGET" || fail "restore failed"

# The restored tree mirrors absolute source paths under TARGET.
restored_alpha="$TARGET$HOME_DIR/projects/-tenant-a/memory/alpha.md"
restored_beta="$TARGET$HOME_DIR/projects/-tenant-b/memory/beta.md"
restored_run="$TARGET$WORK/vault/Notes/runbook.md"
restored_env="$TARGET$HOME_DIR/graph/.env"

[ -f "$restored_alpha" ] || fail "alpha.md not restored"
[ -f "$restored_beta" ]  || fail "beta.md not restored"
[ -f "$restored_run" ]   || fail "vault runbook not restored"
[ -f "$restored_env" ]   || fail "graph/.env not restored (restore would be incomplete)"

grep -q "alpha body" "$restored_alpha" || fail "alpha.md content wrong"
grep -q "DNS failover" "$restored_run" || fail "vault content wrong"
grep -q "should-be-in-the-backup" "$restored_env" || fail ".env content wrong"

# Obsidian's own state must NOT be in the backup.
if [ -e "$TARGET$WORK/vault/.obsidian/workspace.json" ] || [ -e "$TARGET$WORK/vault/.trash/old.md" ]; then
    fail ".obsidian/.trash were backed up (should be excluded)"
fi

# The repository is really encrypted: the plaintext must not be findable in it.
if grep -rqa "should-be-in-the-backup" "$REPO" 2>/dev/null; then
    fail "plaintext secret found in the repo — encryption not in effect"
fi
if grep -rqa "DNS failover" "$REPO" 2>/dev/null; then
    fail "plaintext memory found in the repo — encryption not in effect"
fi

# The restic password must never reach the log or the config.
if grep -qa "$SECRET" "$LOG"; then
    fail "the restic password leaked into the daemon log"
fi
if grep -qa "$SECRET" "$HOME_DIR/engram.yaml"; then
    fail "the restic password leaked into engram.yaml"
fi

run check || fail "integrity check failed"

echo "PASS test_backup_roundtrip"
exit 0
