"""memory_graph_recall.py — Phase 3: hybrid recall.

Instead of dumping the whole index into context, embed the task/query, hybrid-
search the graph for relevant facts, map them back to the canonical memories they
came from, and pull 1-hop [[link]] neighbors. Emits a compact markdown block
suitable for SessionStart injection.

Usage:
  python3 memory_graph_recall.py "<query>" [--k 8]
"""
import asyncio
import logging
import sys
from collections import defaultdict
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from mg_config import build_graphiti


async def recall(query: str, k: int = 8) -> str:
    g = build_graphiti()
    try:
        edges = await g.search(query, num_results=k * 3)
        fact_by_ep = defaultdict(list)
        for e in edges:
            for u in (getattr(e, "episodes", None) or []):
                fact_by_ep[u].append(e.fact)
        if not fact_by_ep:
            return f"_(no graph matches for: {query})_"

        recs, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic) WHERE e.uuid IN $u "
            "RETURN e.uuid AS uuid, e.file AS file, e.fm_name AS name, e.fm_description AS desc",
            u=list(fact_by_ep.keys()),
        )
        meta = {r["uuid"]: r for r in recs}
        ranked = sorted(fact_by_ep, key=lambda u: -len(fact_by_ep[u]))[:k]

        # 1-hop [[link]] neighbours of the recalled memories
        nbrs, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic)-[:LINKS_TO]-(n:Episodic) WHERE e.uuid IN $u "
            "RETURN DISTINCT n.fm_name AS name LIMIT 12",
            u=ranked,
        )
        neighbours = [r["name"] for r in nbrs if r["name"]]

        out = [f"## Recalled memories (top {len(ranked)} for: {query})", ""]
        for u in ranked:
            m = meta.get(u, {})
            name = m.get("name") or m.get("file") or u
            desc = (m.get("desc") or "").strip()
            out.append(f"- **{name}** — {desc}")
            for fact in fact_by_ep[u][:2]:
                out.append(f"    ↳ {fact}")
        if neighbours:
            out += ["", "_related (1-hop): " + ", ".join(sorted(set(neighbours))) + "_"]
        return "\n".join(out)
    finally:
        await g.close()


async def _main():
    args = sys.argv[1:]
    if not args:
        print('usage: memory_graph_recall.py "<query>" [--k N]'); return
    k = int(args[args.index("--k") + 1]) if "--k" in args else 8
    query = " ".join(a for a in args if not a.startswith("--") and a != str(k))
    print(await recall(query, k))


if __name__ == "__main__":
    asyncio.run(_main())
