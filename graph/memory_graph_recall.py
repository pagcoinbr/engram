"""memory_graph_recall.py — Phase 3: hybrid recall.

Instead of dumping the whole index into context, embed the task/query, hybrid-
search the graph for relevant facts, map them back to the canonical memories they
came from, and pull 1-hop [[link]] neighbors. Emits a compact markdown block
suitable for SessionStart injection, or a JSON record list (--json) consumed by
the GUI's hybrid-recall fusion.

Usage:
  python3 memory_graph_recall.py "<query>" [--k 8] [--json]
"""
import asyncio
import json
import logging
import sys
from collections import defaultdict
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from mg_config import build_graphiti


async def recall_records(query: str, k: int = 8) -> dict:
    """Structured recall: returns {records:[{file,name,desc,facts}], neighbours:[...]}
    ranked best-first (by fact count, duplicate files collapsed). The shared core for
    both the markdown view (recall) and the GUI's graph leg (--json)."""
    g = build_graphiti()
    try:
        edges = await g.search(query, num_results=k * 3)
        fact_by_ep = defaultdict(list)
        for e in edges:
            for u in (getattr(e, "episodes", None) or []):
                fact_by_ep[u].append(e.fact)
        if not fact_by_ep:
            return {"records": [], "neighbours": []}

        recs, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic) WHERE e.uuid IN $u "
            "RETURN e.uuid AS uuid, e.file AS file, e.fm_name AS name, "
            "e.fm_description AS desc, e.fm_type AS type",
            u=list(fact_by_ep.keys()),
        )
        meta = {r["uuid"]: r for r in recs}
        ranked = sorted(fact_by_ep, key=lambda u: -len(fact_by_ep[u]))

        records, seen = [], set()
        for u in ranked:
            m = meta.get(u, {})
            f = m.get("file") or u
            if f in seen:                       # collapse multiple episodes of one file
                continue
            seen.add(f)
            records.append({"file": f, "name": m.get("name") or f,
                            "desc": (m.get("desc") or "").strip(),
                            "type": m.get("type") or "",
                            "facts": list(fact_by_ep[u])})
            if len(records) >= k:
                break

        nbrs, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic)-[:LINKS_TO]-(n:Episodic) WHERE e.uuid IN $u "
            "RETURN DISTINCT n.fm_name AS name LIMIT 12",
            u=[r for r in ranked][: len(records)],
        )
        neighbours = sorted({r["name"] for r in nbrs if r["name"]})
        return {"records": records, "neighbours": neighbours}
    finally:
        await g.close()


def _format(query: str, data: dict) -> str:
    records = data.get("records", [])
    if not records:
        return f"_(no graph matches for: {query})_"
    out = [f"## Recalled memories (top {len(records)} for: {query})", ""]
    for r in records:
        out.append(f"- **{r['name']}** — {r['desc']}")
        for fact in r["facts"][:2]:
            out.append(f"    ↳ {fact}")
    if data.get("neighbours"):
        out += ["", "_related (1-hop): " + ", ".join(data["neighbours"]) + "_"]
    return "\n".join(out)


async def recall(query: str, k: int = 8) -> str:
    return _format(query, await recall_records(query, k))


async def _main():
    args = sys.argv[1:]
    if not args:
        print('usage: memory_graph_recall.py "<query>" [--k N] [--json]'); return
    k = int(args[args.index("--k") + 1]) if "--k" in args else 8
    as_json = "--json" in args
    query = " ".join(a for a in args if not a.startswith("--") and a != str(k))
    data = await recall_records(query, k)
    print(json.dumps(data["records"]) if as_json else _format(query, data))


if __name__ == "__main__":
    asyncio.run(_main())
