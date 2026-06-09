"""memory_graph_reconcile.py — Phase 6: temporal reconciliation (gated).

Graphiti sets invalid_at on a relationship-fact when a newer fact contradicts it
(bi-temporal model). This surfaces those superseded facts and the canonical
memories that carry them, so stale facts stop being recalled and the source .md
can be marked SUPERSEDED. REPORT ONLY — never edits .md unattended (your
deterministic gate / human stays in the loop), matching your existing
SUPERSEDED-by-hand convention but automating the detection.

Usage: python3 memory_graph_reconcile.py [--json]
"""
import asyncio
import json
import logging
import sys
from collections import defaultdict

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from mg_config import build_graphiti


async def main():
    as_json = "--json" in sys.argv
    g = build_graphiti()
    try:
        recs, _, _ = await g.driver.execute_query(
            "MATCH (n:Entity)-[e:RELATES_TO]->(m:Entity) WHERE e.invalid_at IS NOT NULL "
            "RETURN e.fact AS fact, toString(e.valid_at) AS valid_at, "
            "toString(e.invalid_at) AS invalid_at, e.episodes AS episodes "
            "ORDER BY e.invalid_at DESC"
        )
        # map episodes -> memory names
        ep_uuids = sorted({u for r in recs for u in (r["episodes"] or [])})
        mem = {}
        if ep_uuids:
            mrecs, _, _ = await g.driver.execute_query(
                "MATCH (e:Episodic) WHERE e.uuid IN $u RETURN e.uuid AS uuid, e.fm_name AS name",
                u=ep_uuids,
            )
            mem = {r["uuid"]: r["name"] for r in mrecs}

        items = []
        by_mem = defaultdict(list)
        for r in recs:
            names = sorted({mem.get(u, u) for u in (r["episodes"] or [])})
            items.append({"fact": r["fact"], "valid_at": r["valid_at"],
                          "invalid_at": r["invalid_at"], "memories": names})
            for nm in names:
                by_mem[nm].append(r["fact"])

        if as_json:
            print(json.dumps({"superseded": items, "by_memory": by_mem}, indent=2)); return

        print(f"# Reconcile — {len(items)} superseded fact(s) across {len(by_mem)} memory(ies)\n")
        for nm, facts in sorted(by_mem.items(), key=lambda x: -len(x[1])):
            print(f"## {nm}  ({len(facts)} superseded)")
            for f in facts[:5]:
                print(f"  - {f}")
            print("  → PROPOSE: mark this .md SUPERSEDED / refresh (gated; not auto-applied)\n")
        if not items:
            print("(no superseded facts yet — nothing to reconcile)")
    finally:
        await g.close()


if __name__ == "__main__":
    asyncio.run(main())
