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
import mg_state     # per-slug scoping of the extraction cache + insert/sync state


def _slug() -> str:
    """Which store to sync. --slug wins, then the environment, then $HOME.

    A flag is needed because a tenanted host has several stores and this script
    is invoked once per store by the daemon; deriving from the environment alone
    meant every tenant's pass read the same directory.
    """
    a = sys.argv[1:]
    if "--slug" in a:
        i = a.index("--slug")
        if i + 1 < len(a) and not a[i + 1].startswith("--"):
            return a[i + 1]
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")


def _tenant():
    """The identity to insert as, or None on a pre-tenancy install.

    Passed through to memory_graph_insert.py, which refuses to write without it
    once tenants are configured — writing into the shared group is what made the
    graph leg cross projects.
    """
    a = sys.argv[1:]
    if "--tenant" in a:
        i = a.index("--tenant")
        if i + 1 < len(a) and not a[i + 1].startswith("--"):
            return a[i + 1]
    return os.environ.get("ENGRAM_TENANT") or None

_SLUG = _slug()
MEM_DIR = Path.home() / ".claude" / "projects" / _SLUG / "memory"
# Per-slug (state/<slug>/…): the daemon runs this once per store, so a shared
# cache/state made an insert for one store skip another store's files and
# re-extract them every cycle — a 24/7 local-LLM loop. See mg_state.
EXTRACT_DIR, INSERT_STATE, SYNC_STATE = mg_state.paths(HERE, _SLUG)
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
    EXTRACT_DIR.mkdir(parents=True, exist_ok=True)   # state/<slug>/extractions
    done, sync, files = _done_files(), _load_sync(), _store_files()
    new = [p for p in files if p.name not in done]
    changed = [p for p in files if p.name in done and sync.get(p.name) != _sha(p)]
    if changed:
        head = ", ".join(p.name for p in changed[:5]) + (" ..." if len(changed) > 5 else "")
        print(f"[sync] {len(changed)} changed memory(ies) — run "
              f"`graph_maint.py --refresh-changed --apply`, then `graph_sync.py --insert`: {head}")
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
            extracted.append(p.name)
            print(f"[sync] extracted {p.name}: {len(data['entities'])} entities, {len(data['edges'])} edges")
        except Exception as e:
            print(f"[sync] extract FAILED {p.name}: {e}", file=sys.stderr)
    # sync_state is stamped by memory_graph_insert.py, per memory, AFTER the episode
    # commits. Stamping it here (at extraction time) marked files as in-graph that
    # the insert below might never reach, so a later edit to them looked unchanged.
    if not extracted:
        print("[sync] nothing extracted; skipping insert")
        return
    print(f"[sync] inserting {len(extracted)} memory(ies) into the graph...")
    _t = _tenant()
    # Tell the insert child WHICH store these extractions came from. Without this it
    # fell back to the $HOME-derived default slug, read the wrong MEM_DIR, and
    # `skip (no .md)`-ed every file of every non-default store — never stamping
    # "done", so the extractor above re-ran forever. Pass --slug AND the env so the
    # child's MEM_DIR and per-slug state match this run exactly.
    env = {**os.environ, "CLAUDE_MEMORY_SLUG": _SLUG}
    r = subprocess.run([GRAPH_PY, str(HERE / "memory_graph_insert.py"), "--slug", _SLUG]
                       + (["--tenant", _t] if _t else [])
                       + ["--only", *extracted], env=env)
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
    if "--migrate-state" in a:
        notes = mg_state.migrate_legacy(HERE)
        print("\n".join(notes) if notes else "[migrate] nothing to migrate (already per-slug)")
        return
    # Fold any pre-tenancy flat state into per-slug dirs before reading it. Idempotent
    # and flock-guarded, so it is a no-op once done and safe across concurrent runs.
    mg_state.migrate_legacy(HERE)
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
