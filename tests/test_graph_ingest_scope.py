#!/usr/bin/env python3
"""Pins the per-slug scoping of the graph ingest state — the fix for the 24/7
local-LLM loop.

The daemon runs the ingest once per tenant slug, each with its own MEM_DIR, but
the extraction cache + insert_state.json + sync_state.json used to be a single
flat copy shared by every store. An insert for store B then read store A's
extraction, failed to find A's `.md` under B's MEM_DIR (`skip (no .md)`), never
stamped "done", and the extractor re-harvested that memory every cycle forever.

This guards the two properties the fix depends on, with NO graphiti/Neo4j needed
(mg_state is stdlib-only):
  * paths() gives each slug a distinct, non-overlapping state location;
  * migrate_legacy() splits a flat insert_state/sync_state into per-slug state,
    assigns each done file to the store it was inserted from (sha-disambiguated
    when a same-named file exists in two stores), keeps the entity map with the
    largest store, retires the flat files, and is idempotent.
"""
import hashlib
import importlib.util
import json
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MOD = ROOT / "graph" / "mg_state.py"

FAILURES = []


def check(label, got, want):
    if got != want:
        FAILURES.append(f"{label}: got {got!r}, want {want!r}")


def load():
    spec = importlib.util.spec_from_file_location("mg_state_test", MOD)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _write(path: Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return hashlib.sha256(text.encode()).hexdigest()


def main():
    with tempfile.TemporaryDirectory() as home:
        os.environ["HOME"] = home
        os.environ.pop("CLAUDE_MEMORY_SLUG", None)
        sys.argv = ["mg_state_test"]            # no --slug
        mg = load()

        # 0. slug resolution — the mechanism the inserter uses to learn WHICH store
        #    graph_sync extracted for. --slug (what graph_sync now forwards) wins,
        #    then CLAUDE_MEMORY_SLUG. Getting this wrong is the original bug: the
        #    child fell back to the $HOME default and skipped every non-root file.
        sys.argv = ["x", "--slug", "-root-bbhost", "--tenant", "bbhost", "--only", "a.md"]
        check("slug_from_argv honors --slug", mg.slug_from_argv_env(), "-root-bbhost")
        sys.argv = ["x"]
        os.environ["CLAUDE_MEMORY_SLUG"] = "-root-envslug"
        check("slug_from_argv falls back to env", mg.slug_from_argv_env(), "-root-envslug")
        os.environ.pop("CLAUDE_MEMORY_SLUG", None)
        sys.argv = ["mg_state_test"]

        here = Path(home) / ".claude" / "graph"
        projects = Path(home) / ".claude" / "projects"

        # Two stores. "shared.md" exists in BOTH with different content, so the
        # migration must use the recorded sha to assign it correctly.
        sha_a_only = _write(projects / "-root-A" / "memory" / "a_only.md", "alpha content")
        sha_b_only = _write(projects / "-root-B" / "memory" / "b_only.md", "beta content")
        sha_shared_a = _write(projects / "-root-A" / "memory" / "shared.md", "shared in A")
        sha_shared_b = _write(projects / "-root-B" / "memory" / "shared.md", "shared in B")

        # 1. paths() are per-slug and disjoint.
        ea, ia, sa = mg.paths(here, "-root-A")
        eb, ib, sb = mg.paths(here, "-root-B")
        check("insert_state per slug differs", ia != ib, True)
        assert "-root-A" in str(ia) and "-root-B" in str(ib)
        check("extractions nested under slug", ea.parent == ia.parent, True)

        # 2. Seed a FLAT legacy state: 3 done files + their shas. a_only and
        #    shared(sha of A's copy) belong to A; b_only belongs to B. Plus a done
        #    entry for a file no store has (an orphan to drop).
        here.mkdir(parents=True, exist_ok=True)
        (here / "insert_state.json").write_text(json.dumps({
            "done": {"a_only.md": "uuid-a", "shared.md": "uuid-s",
                     "b_only.md": "uuid-b", "ghost.md": "uuid-g"},
            "entities": {"server-a": "ent-1", "api": "ent-2"},
        }))
        (here / "sync_state.json").write_text(json.dumps({
            "a_only.md": sha_a_only, "shared.md": sha_shared_a,
            "b_only.md": sha_b_only, "ghost.md": "deadbeef",
        }))
        (here / "extractions").mkdir()
        (here / "extractions" / "a_only.json").write_text(json.dumps({"file": "a_only.md"}))
        (here / "extractions" / "b_only.json").write_text(json.dumps({"file": "b_only.md"}))

        notes = mg.migrate_legacy(here)
        assert notes, "migration produced no notes"

        da = json.loads(ia.read_text())
        db = json.loads(ib.read_text())
        # 3. Each done file lands in the store it came from; the sha picks A's copy
        #    of shared.md over B's.
        check("A owns a_only", "a_only.md" in da["done"], True)
        check("A owns shared (by sha)", "shared.md" in da["done"], True)
        check("B owns b_only", "b_only.md" in db["done"], True)
        check("B did NOT get shared", "shared.md" not in db["done"], True)
        check("ghost dropped from A", "ghost.md" not in da["done"], True)
        check("ghost dropped from B", "ghost.md" not in db["done"], True)
        # 4. Entity map stays with the larger store (A has 2 done, B has 1).
        check("entities with larger store", da["entities"], {"server-a": "ent-1", "api": "ent-2"})
        check("smaller store entities empty", db["entities"], {})
        # 5. sync_state carried across, per slug.
        check("A sync has shared sha", json.loads(sa.read_text()).get("shared.md"), sha_shared_a)
        # 6. Cached extractions moved to their owner.
        check("a_only extraction moved to A", (ea / "a_only.json").exists(), True)
        check("b_only extraction moved to B", (eb / "b_only.json").exists(), True)
        # 7. Flat files retired.
        check("flat insert_state retired", (here / "insert_state.json").exists(), False)
        check("flat insert_state kept as .migrated", (here / "insert_state.json.migrated").exists(), True)
        # 8. Idempotent: a second run is a no-op and must not corrupt per-slug state.
        again = mg.migrate_legacy(here)
        check("second migrate is a no-op", again, [])
        check("A done intact after 2nd run", json.loads(ia.read_text())["done"].get("a_only.md"), "uuid-a")

    if FAILURES:
        print("FAIL test_graph_ingest_scope")
        for line in FAILURES:
            print("  -", line)
        return 1
    print("PASS test_graph_ingest_scope")
    return 0


if __name__ == "__main__":
    sys.exit(main())
