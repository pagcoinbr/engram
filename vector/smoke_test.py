#!/usr/bin/env python3
"""smoke_test.py — prove the Qdrant round-trip end to end (parallel to
graph/smoke_test.py). Requires: Qdrant reachable + embeddings reachable +
vector_store.enabled (or ENGRAM env overrides). Uses a throwaway collection so it
never touches your real index.

Run:
  ENGRAM_VECTOR_COLLECTION=engram_smoke vector/venv/bin/python vector/smoke_test.py
(or just `python vector/smoke_test.py` once configured).
"""
from __future__ import annotations
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import vector_config as vc
from vector_store import EngramVectorStore

SAMPLES = [
    # (filename, name, description) — two are intentional near-dupes.
    ("project_acme-api.md", "Acme API service",
     "The Acme API runs on api-1, a FastAPI service behind nginx talking to postgres."),
    ("project_acme-api-2.md", "Acme API backend",
     "Acme's API is a FastAPI app on host api-1, fronted by nginx, backed by postgres."),
    ("reference_postgres.md", "Postgres gotchas",
     "Postgres connection pooling via pgbouncer; migrations run with alembic upgrade head."),
]


def main() -> int:
    try:
        store = EngramVectorStore()
    except vc.VectorUnavailable as e:
        print(f"SKIP — vector store unavailable: {e}")
        print("      (enable vector_store + start Qdrant: cd ~/.claude/vector && docker compose up -d)")
        return 0

    failures = []

    def check(label, cond):
        print(f"  [{'PASS' if cond else 'FAIL'}] {label}")
        if not cond:
            failures.append(label)

    print(f"collection: {store.collection}  dim: {store.dim}  url: {vc.url()}")

    # Use a fresh collection so the smoke test is hermetic.
    store.ensure_collection(recreate=True)
    check("ensure_collection (recreate)", store.client.collection_exists(store.collection))

    for fn, nm, ds in SAMPLES:
        store.upsert(filename=fn, name=nm, description=ds, mtype="project", sha="deadbeef")
    check("upsert 3 memories", store.stats()["points"] == 3)

    hits = store.search("which host runs the FastAPI service?", k=2)
    top = hits[0]["file"] if hits else None
    check(f"search returns an Acme API memory (got {top})",
          top in ("project_acme-api.md", "project_acme-api-2.md"))

    dupes = store.find_duplicates(threshold=0.80)
    dupe_pair = {f for _, a, b in dupes for f in (a, b)}
    check(f"find_duplicates flags the near-dupe pair (got {len(dupes)} pair(s))",
          {"project_acme-api.md", "project_acme-api-2.md"} <= dupe_pair)

    store.delete("reference_postgres.md")
    check("delete one memory", store.stats()["points"] == 2)

    # Clean up the throwaway collection.
    store.client.delete_collection(store.collection)
    check("teardown collection", not store.client.collection_exists(store.collection))

    print("\n" + ("ALL PASSED" if not failures else f"FAILED: {failures}"))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
