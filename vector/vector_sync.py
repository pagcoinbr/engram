#!/usr/bin/env python3
"""vector_sync.py — keep the OPTIONAL Qdrant index in sync with the .md store.

The .md store is the source of truth; Qdrant is a rebuildable semantic index over
it (parallels graph_sync.py for Neo4j). Sha-synced: only new/changed files get
re-embedded. Every command is a clean no-op when the vector store is disabled or
Qdrant is unreachable, so save/delete hooks and the daemon never fail because of it.

  --insert [--only F.md ...]  embed+upsert NEW/CHANGED memories; drop points whose
                              .md no longer exists. --only limits to named files
                              (used by the save hook for a cheap single-file upsert).
  --rebuild                   drop + recreate the collection, re-embed every .md.
  --delete <F.md>             remove one memory's point (used by the delete hook).
  --status                    counts: store memories / indexed / pending.
"""
from __future__ import annotations
import hashlib
import json
import os
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "bin"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import memory_ai
import vector_config as vc
from vector_store import EngramVectorStore

SYNC_STATE = HERE / "sync_state.json"   # file -> sha256(.md) last indexed


def _slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")

MEM_DIR = Path.home() / ".claude" / "projects" / _slug() / "memory"


def _store_files() -> list[Path]:
    if not MEM_DIR.exists():
        return []
    return sorted(p for p in MEM_DIR.glob("*.md") if p.name != "MEMORY.md")

def _sha(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()

def _load_sync() -> dict:
    return json.loads(SYNC_STATE.read_text()) if SYNC_STATE.exists() else {}

def _save_sync(d: dict) -> None:
    SYNC_STATE.write_text(json.dumps(d, indent=1))

def _frontmatter(p: Path) -> tuple[str, str, str]:
    """Pull name / description / type from frontmatter (tolerant of the nested
    `metadata:` form). Falls back to the filename stem for name."""
    t = p.read_text(errors="ignore")
    def field(key):
        m = re.search(rf"^\s*{key}:\s*(.+)$", t, re.M)
        return m.group(1).strip().strip('"\'') if m else ""
    return field("name") or p.stem, field("description"), field("type")


def _store():
    """Build the vector store or raise VectorUnavailable (the caller decides)."""
    cfg = memory_ai.load()
    if not memory_ai.vector_enabled(cfg):
        raise vc.VectorUnavailable("vector_store disabled (or local_enabled false)")
    s = EngramVectorStore(cfg)
    s.ensure_collection()
    return s


def cmd_insert(only=None) -> int:
    try:
        store = _store()
    except vc.VectorUnavailable as e:
        print(f"[vector] skip insert — {e}")
        return 0
    files = _store_files()
    if only:
        wanted = set(only)
        files = [p for p in files if p.name in wanted]
    sync = _load_sync()
    present = {p.name for p in _store_files()}

    # Drop points whose .md is gone (only on a full run, not a scoped --only run).
    if not only:
        for gone in [f for f in sync if f not in present]:
            try:
                store.delete(gone)
                print(f"[vector] removed stale {gone}")
            except Exception as e:
                print(f"[vector] remove FAILED {gone}: {e}", file=sys.stderr)
            sync.pop(gone, None)

    changed = [p for p in files if sync.get(p.name) != _sha(p)]
    if not changed:
        print("[vector] nothing to index" if not only else f"[vector] {','.join(only)} already current")
        _save_sync(sync)
        return 0
    n = 0
    for p in changed:
        try:
            name, desc, mtype = _frontmatter(p)
            sha = _sha(p)
            store.upsert(filename=p.name, name=name, description=desc, mtype=mtype, sha=sha)
            sync[p.name] = sha
            n += 1
            print(f"[vector] indexed {p.name}")
        except vc.VectorUnavailable as e:
            print(f"[vector] aborted — Qdrant unreachable mid-run: {e}", file=sys.stderr)
            break
        except Exception as e:
            print(f"[vector] index FAILED {p.name}: {e}", file=sys.stderr)
    _save_sync(sync)
    print(f"[vector] indexed {n} memory(ies)")
    return 0


def cmd_rebuild() -> int:
    try:
        cfg = memory_ai.load()
        if not memory_ai.vector_enabled(cfg):
            print("[vector] skip rebuild — vector_store disabled")
            return 0
        store = EngramVectorStore(cfg)
        store.ensure_collection(recreate=True)
    except vc.VectorUnavailable as e:
        print(f"[vector] skip rebuild — {e}")
        return 0
    sync = {}
    n = 0
    for p in _store_files():
        try:
            name, desc, mtype = _frontmatter(p)
            sha = _sha(p)
            store.upsert(filename=p.name, name=name, description=desc, mtype=mtype, sha=sha)
            sync[p.name] = sha
            n += 1
        except Exception as e:
            print(f"[vector] index FAILED {p.name}: {e}", file=sys.stderr)
    _save_sync(sync)
    print(f"[vector] rebuilt collection — {n} memory(ies) indexed")
    return 0


def cmd_delete(filename: str) -> int:
    try:
        store = _store()
    except vc.VectorUnavailable as e:
        print(f"[vector] skip delete — {e}")
        return 0
    try:
        store.delete(filename)
        sync = _load_sync()
        sync.pop(filename, None)
        _save_sync(sync)
        print(f"[vector] deleted {filename}")
    except Exception as e:
        print(f"[vector] delete FAILED {filename}: {e}", file=sys.stderr)
    return 0


def cmd_status() -> int:
    files = _store_files()
    cfg = memory_ai.load()
    if not memory_ai.vector_enabled(cfg):
        print(f"store memories: {len(files)}")
        print("vector store:   disabled (pure-markdown fallback)")
        return 0
    try:
        store = _store()
        st = store.stats()
        sync = _load_sync()
        pending = [p.name for p in files if sync.get(p.name) != _sha(p)]
        print(f"store memories: {len(files)}")
        print(f"indexed:        {st['points']}  (collection '{st['collection']}', dim {st['dim']})")
        print(f"pending insert: {len(pending)}")
        if pending:
            print("  " + ", ".join(pending[:10]) + (" ..." if len(pending) > 10 else ""))
    except vc.VectorUnavailable as e:
        print(f"store memories: {len(files)}")
        print(f"vector store:   UNREACHABLE — {e}")
    return 0


def main() -> int:
    a = sys.argv[1:]
    if "--status" in a:
        return cmd_status()
    if "--rebuild" in a:
        return cmd_rebuild()
    if "--delete" in a:
        i = a.index("--delete")
        if i + 1 >= len(a):
            print("usage: vector_sync.py --delete <file.md>", file=sys.stderr)
            return 2
        return cmd_delete(a[i + 1])
    if "--insert" in a:
        only = None
        if "--only" in a:
            only = [x for x in a[a.index("--only") + 1:] if not x.startswith("-")]
        return cmd_insert(only)
    print(__doc__)
    return 0


if __name__ == "__main__":
    sys.exit(main())
