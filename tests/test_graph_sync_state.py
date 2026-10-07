#!/usr/bin/env python3
"""Regression: sync_state.json means "the graph holds THIS version of the file".

It is the only thing that tells graph_sync a memory has drifted from the graph, so
a wrong entry is invisible — the memory simply never gets refreshed again. Two ways
that happened, both fixed here and guarded below:

  1. memory_graph_insert.py stamped a sha for EVERY .md in the store at the end of
     a run, including files that were skipped, filtered out by --only, or never
     reached because the run died.
  2. graph_sync.py stamped the sha at EXTRACTION time, before the insert subprocess
     that might never commit it.

The insert path needs graphiti_core + mg_config, so the module-level tests skip
when those are absent (CI without the graph venv); the source guards always run.
"""
import importlib.util
import json
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GRAPH = ROOT / "graph"


def test_source_guards():
    """Cheap, dependency-free, and aimed squarely at the two regressions."""
    insert = (GRAPH / "memory_graph_insert.py").read_text()
    sync = (GRAPH / "graph_sync.py").read_text()

    assert 'MEM_DIR.glob("*.md")' not in insert.split("def main")[-1], \
        "memory_graph_insert stamps sync_state from the whole store again"
    assert "save_sync_state(sync)" in insert, \
        "memory_graph_insert no longer stamps sync_state per inserted memory"
    # the stamp must live next to the insert_state save, i.e. after the episode
    # committed, not after the loop
    body = insert.split("st[\"done\"][fname] = ep.uuid")[-1]
    assert "save_sync_state" in body.split("print(")[0], \
        "the sync_state stamp drifted away from the per-memory commit point"

    assert "_save_sync" not in sync, \
        "graph_sync writes sync_state again — only the insert may stamp it"
    print("ok — sync_state is stamped per committed insert, and only there")


def load_insert_module():
    sys.path.insert(0, str(Path.home() / ".claude" / "graph"))
    spec = importlib.util.spec_from_file_location(
        "memory_graph_insert", GRAPH / "memory_graph_insert.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def test_sync_state_merges(mod, tmp):
    """A run must MERGE into the previous state. Overwriting it wholesale drops the
    entries of every memory this run did not touch, which re-queues them forever."""
    mod.SYNC_STATE = Path(tmp) / "sync_state.json"
    mod.SYNC_STATE.write_text(json.dumps({"old.md": "sha-old"}))

    sync = mod.load_sync_state()
    assert sync == {"old.md": "sha-old"}, sync
    sync["new.md"] = "sha-new"
    mod.save_sync_state(sync)

    again = json.loads(mod.SYNC_STATE.read_text())
    assert again == {"old.md": "sha-old", "new.md": "sha-new"}, again

    # a missing or corrupt state file must read as empty, not explode: the insert
    # runs unattended from the daemon
    mod.SYNC_STATE.write_text("{ not json")
    assert mod.load_sync_state() == {}
    mod.SYNC_STATE.unlink()
    assert mod.load_sync_state() == {}
    print("ok — sync_state merges across runs and survives a corrupt state file")


def main():
    test_source_guards()
    try:
        mod = load_insert_module()
    except Exception as e:
        print(f"skip — graph insert module unavailable ({type(e).__name__}: {e})")
        return
    with tempfile.TemporaryDirectory() as d:
        test_sync_state_merges(mod, d)


if __name__ == "__main__":
    main()
