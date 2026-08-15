#!/usr/bin/env python3
"""graph_sync.py — the engram graph<->.md auto-link orchestrator.

The .md store is the source of truth; the Neo4j graph is a continuously-synced
associative + temporal index over it. This wires the previously-manual steps into
one incremental command the daemon runs on a cadence:

  --insert [--limit N]  NEW .md memories -> extract entities/edges (via engram_llm,
                        per extract_spec.md) -> insert (memory_graph_insert.py).
  --export [--verify]   regenerate .md from the graph (memory_graph_export.py);
                        --verify only checks byte-exact round-trip + reports drift.
  --reconcile           surface graph-detected superseded facts (memory_graph_reconcile.py).
  --all                 insert, then export --verify, then reconcile.
  --status              counts: store memories / in graph / pending.

Authority model (v1): .md-authoritative. Insert handles NEW files; CHANGED files
are only REPORTED here — refresh them with `graph_maint.py --refresh-changed`,
which deletes the stale episode before re-inserting.

⚠ Do NOT use `memory_graph_insert.py --rebuild` to refresh: it resets the local
state file and deletes NOTHING in Neo4j, so it mints a SECOND episode for every
memory and duplicates the graph. (This docstring used to claim the opposite; that
is how 126 duplicate episodes accumulated.) --rebuild is only for a graph that has
been wiped, and now refuses to run against a populated one without --force.

Extraction needs only the LLM (works on either backend); the insert/export/
reconcile subprocesses need the graph venv (Graphiti + Neo4j).
"""
import copy
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "bin"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import engram_llm  # generation routed by backend (ollama | claude)
import memory_ai   # config loader, for the per-attempt temperature override


def _slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")

MEM_DIR = Path.home() / ".claude" / "projects" / _slug() / "memory"
EXTRACT_DIR = HERE / "extractions"
INSERT_STATE = HERE / "insert_state.json"
SYNC_STATE = HERE / "sync_state.json"          # file -> sha256(.md) last extracted
SPEC = HERE / "extract_spec.md"

# graphiti lives in an isolated venv; run the insert/export/reconcile subprocesses
# with THAT python (this orchestrator itself only needs engram_llm, no graphiti).
GRAPH_PY = os.environ.get("ENGRAM_GRAPH_PYTHON") or str(HERE / "venv" / "bin" / "python")
if not Path(GRAPH_PY).exists():
    GRAPH_PY = sys.executable


def _store_files():
    if not MEM_DIR.exists():
        return []
    return sorted(p for p in MEM_DIR.glob("*.md")
                  if p.name not in ("MEMORY.md", "MEMORY_FULL.md"))

def _done_files() -> set:
    if not INSERT_STATE.exists():
        return set()
    return set(json.loads(INSERT_STATE.read_text()).get("done", {}).keys())

def _sha(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()

def _load_sync() -> dict:
    return json.loads(SYNC_STATE.read_text()) if SYNC_STATE.exists() else {}

def _save_sync(d: dict):
    SYNC_STATE.write_text(json.dumps(d, indent=1))


def _parse_json(raw: str) -> dict:
    """Pull a JSON object out of an LLM response (tolerate code fences / prose)."""
    raw = raw.strip()
    if raw.startswith("```"):
        raw = re.sub(r"^```[a-zA-Z]*\n?", "", raw)
        raw = re.sub(r"\n?```$", "", raw).strip()
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        i, j = raw.find("{"), raw.rfind("}")
        if i != -1 and j > i:
            return json.loads(raw[i:j + 1])
        raise


def extract(md_path: Path) -> dict:
    spec = SPEC.read_text()
    content = md_path.read_text(errors="ignore")
    prompt = (f"{spec}\n\n---\nFILE: {md_path.name}\n---\n{content}\n\n"
              "Output ONLY the JSON object described above — no prose, no code fences.")
    # For some inputs the model falls into a degenerate non-JSON reply and repeats it
    # BYTE-IDENTICALLY at the configured temperature — measured 3/3 the same 52-char
    # string on reference_cipher_signer_remote_cutover.md (2026-08-15). So retrying the
    # same call is provably useless; vary the sampling instead. Stays on the local
    # backend by design — no fallback provider.
    last = None
    for temp in (None, 0.6, 1.0):
        cfg = None
        if temp is not None:
            cfg = copy.deepcopy(memory_ai.load())
            cfg.setdefault("ollama", {})["temperature"] = temp
        try:
            data = _parse_json(engram_llm.generate(prompt, role="harvest", cfg=cfg))
            if temp is not None:
                print(f"[sync] {md_path.name}: extraction recovered at temperature {temp}",
                      flush=True)
            break
        except Exception as e:                      # non-JSON reply or transport error
            last = e
    else:
        raise last
    data.setdefault("file", md_path.name)
    data.setdefault("entities", [])
    data.setdefault("edges", [])
    return data


def cmd_insert(limit=None):
    EXTRACT_DIR.mkdir(exist_ok=True)
    done, sync, files = _done_files(), _load_sync(), _store_files()
    new = [p for p in files if p.name not in done]
    changed = [p for p in files if p.name in done and sync.get(p.name) != _sha(p)]
    if changed:
        head = ", ".join(p.name for p in changed[:5]) + (" ..." if len(changed) > 5 else "")
        print(f"[sync] {len(changed)} changed memory(ies) — re-run "
              f"`memory_graph_insert.py --rebuild` to refresh: {head}")
    if limit:
        new = new[:limit]
    if not new:
        print("[sync] no new memories to insert")
        return
    extracted = []
    for p in new:
        try:
            data = extract(p)
            (EXTRACT_DIR / (p.stem + ".json")).write_text(json.dumps(data, indent=1))
            sync[p.name] = _sha(p)
            extracted.append(p.name)
            print(f"[sync] extracted {p.name}: {len(data['entities'])} entities, {len(data['edges'])} edges")
        except Exception as e:
            print(f"[sync] extract FAILED {p.name}: {e}", file=sys.stderr)
    _save_sync(sync)
    if not extracted:
        print("[sync] nothing extracted; skipping insert")
        return
    print(f"[sync] inserting {len(extracted)} memory(ies) into the graph...")
    r = subprocess.run([GRAPH_PY, str(HERE / "memory_graph_insert.py"), "--only", *extracted])
    if r.returncode:
        print(f"[sync] insert exited {r.returncode}", file=sys.stderr)
        sys.exit(r.returncode)
    print("[sync] insert complete")


def cmd_export(verify=False, no_git=False):
    args = [GRAPH_PY, str(HERE / "memory_graph_export.py")]
    if verify:
        args.append("--verify")
    if no_git:
        args.append("--no-git")
    subprocess.run(args)


def cmd_reconcile():
    subprocess.run([GRAPH_PY, str(HERE / "memory_graph_reconcile.py")])


def cmd_status():
    files, done = _store_files(), _done_files()
    pending = [p.name for p in files if p.name not in done]
    print(f"store memories: {len(files)}")
    print(f"in graph:       {len(done)}")
    print(f"pending insert: {len(pending)}")
    if pending:
        print("  " + ", ".join(pending[:10]) + (" ..." if len(pending) > 10 else ""))


def main():
    a = sys.argv[1:]
    if "--status" in a:
        return cmd_status()
    if "--insert" in a:
        lim = int(a[a.index("--limit") + 1]) if "--limit" in a else None
        return cmd_insert(lim)
    if "--export" in a:
        return cmd_export("--verify" in a, "--no-git" in a)
    if "--reconcile" in a:
        return cmd_reconcile()
    if "--all" in a:
        cmd_insert()
        cmd_export(verify=True)
        cmd_reconcile()
        return
    print(__doc__)


if __name__ == "__main__":
    main()
