#!/usr/bin/env bash
# e2e: compress-merge reversibility — apply moves sources to quarantine + de-indexes;
# undo restores them + re-indexes and trashes the umbrella. Uses the real shell
# scripts in a throwaway HOME, no GitHub. Guards the destructive auto-curate path.
set -uo pipefail
REPO_BIN="$(cd "$(dirname "$0")/../bin" && pwd)"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
CL="$T/.claude"; mkdir -p "$CL"
for f in memory_lib.sh save_memory.sh save_memory_content_only.sh delete_memory.sh \
         engram_secrets.py engram_telegram_gate.py; do cp "$REPO_BIN/$f" "$CL/$f"; done
chmod +x "$CL"/*.sh "$CL"/*.py
export HOME="$T" CLAUDE_MEMORY_SLUG="-m"; unset CLAUDE_MEMORY_REPO TELEGRAM_BOT_TOKEN
MEM="$CL/projects/-m/memory"; mkdir -p "$MEM"
printf -- '---\nname: project_a\ndescription: svc A\nmetadata:\n  type: project\n---\nA runs on port 8080\n' > "$MEM/project_a.md"
printf -- '---\nname: project_b\ndescription: svc B\nmetadata:\n  type: project\n---\nB runs on port 8081\n' > "$MEM/project_b.md"
printf '# Index\n\n## Uncategorized (auto-added)\n- [project_a](project_a.md) — a\n- [project_b](project_b.md) — b\n' > "$MEM/MEMORY.md"

python3 - <<'PY'
import sys,os; sys.path.insert(0,os.path.expanduser("~/.claude"))
import engram_telegram_gate as g
MEM=g.MEM; fail=0
ck=lambda c,m:(print(("  ok" if c else "  FAIL")+f": {m}"), globals().__setitem__('fail', fail or (0 if c else 1)))
params={"umbrella":"project_a.md",
        "umbrella_content":"---\nname: project_a\ndescription: svc A+B\nmetadata:\n  type: project\n---\nA:8080 B:8081 merged\n",
        "desc":"svc A+B","absorbed":["project_b.md"],"merge_id":"txn123"}
ok,_=g._apply_merge(params)
TXN=MEM/'.quarantine/merge-txn123'
ck(ok and "A:8080 B:8081" in (MEM/'project_a.md').read_text(), "umbrella written compressed")
ck((TXN/'project_a.md.orig').is_file(), "umbrella ORIGINAL backed up in txn dir (Fix 1+2)")
ck("port 8080" in (TXN/'project_a.md.orig').read_text(), "backup holds umbrella's original fact")
ck((TXN/'project_b.md').is_file(), "source quarantined in txn dir (not trashed)")
ck('](project_b.md)' not in (MEM/'MEMORY.md').read_text(), "source out of index")
# stacked-merge refusal: a 2nd merge on the same umbrella before undo is refused
ok2,d2=g._apply_merge({**params,"merge_id":"txn999"})
ck(not ok2 and "outstanding" in d2, "stacked merge refused (Fix 2)")
ok,_=g._apply_merge_undo({"umbrella":"project_a.md","absorbed":["project_b.md"],"merge_id":"txn123"})
ck(ok, "undo ok")
ck("A runs on port 8080" in (MEM/'project_a.md').read_text() and "B:8081" not in (MEM/'project_a.md').read_text(),
   "umbrella restored to EXACT original (not compressed)")
ck((MEM/'project_b.md').is_file() and '](project_b.md)' in (MEM/'MEMORY.md').read_text(), "absorbed restored + re-indexed")
ck(not (MEM/'.quarantine/project_a.md.orig').is_file(), "backups cleaned after successful undo")
sys.exit(fail)
PY
rc=$?
[[ $rc == 0 ]] && echo "E2E MERGE/UNDO PASS" || echo "E2E MERGE/UNDO FAIL"
exit $rc
