"""memory_graph_consolidate.py — Phase 7: consolidation + decay (human-memory).

Computes a salience score per canonical memory from graph signals and writes it
back to the node:
  salience = 0.40*centrality + 0.30*recall_freq + 0.20*recency - 0.30*superseded
Consolidation: high-salience, well-connected, valid memories -> e.fixed=true
(strengthened, like sleep consolidation). Forgetting: isolated, never-recalled,
old, superseded -> e.fade_candidate=true (surfaced, NOT deleted — .md removal
stays gated). Only graph properties are written; the .md store is never mutated.

Usage: python3 memory_graph_consolidate.py [--apply] [--top N]
"""
import asyncio
import logging
import sys

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from mg_config import build_graphiti


def norm(v, lo, hi):
    return 0.0 if hi <= lo else max(0.0, min(1.0, (v - lo) / (hi - lo)))


async def main():
    apply = "--apply" in sys.argv
    top = int(sys.argv[sys.argv.index("--top") + 1]) if "--top" in sys.argv else 10
    g = build_graphiti()
    try:
        recs, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic) OPTIONAL MATCH (e)-[r]-() "
            "WITH e, count(r) AS deg "
            "RETURN e.uuid AS uuid, e.fm_name AS name, deg, "
            "coalesce(e.recall_count,0) AS recall, "
            "CASE WHEN e.valid_at IS NULL THEN 3650 "
            "ELSE duration.inDays(date(e.valid_at), date()).days END AS age_days"
        )
        sup, _, _ = await g.driver.execute_query(
            "MATCH (:Entity)-[x:RELATES_TO]->(:Entity) WHERE x.invalid_at IS NOT NULL "
            "UNWIND x.episodes AS eu RETURN eu AS uuid, count(*) AS sc"
        )
        supmap = {r["uuid"]: r["sc"] for r in sup}
        if not recs:
            print("(graph empty — nothing to consolidate yet)"); return

        degs = [r["deg"] for r in recs]
        recalls = [r["recall"] for r in recs]
        dmax, rmax = max(degs), max(recalls)
        rows = []
        for r in recs:
            s_cent = norm(r["deg"], 0, dmax)
            s_recall = norm(r["recall"], 0, rmax)
            s_recency = 1.0 - norm(r["age_days"], 0, 365)         # newer => higher
            s_sup = norm(supmap.get(r["uuid"], 0), 0, 5)
            sal = 0.40 * s_cent + 0.30 * s_recall + 0.20 * s_recency - 0.30 * s_sup
            sal = max(0.0, round(sal, 4))
            rows.append({"uuid": r["uuid"], "name": r["name"], "deg": r["deg"],
                         "recall": r["recall"], "age": r["age_days"],
                         "sup": supmap.get(r["uuid"], 0), "salience": sal})
        rows.sort(key=lambda x: -x["salience"])

        # classification thresholds (relative)
        fixed_cut = rows[max(0, len(rows) // 4 - 1)]["salience"] if rows else 1.0
        fade = [r for r in rows if r["deg"] <= 1 and r["recall"] == 0 and (r["age"] > 180 or r["sup"] > 0)]

        if apply:
            for r in rows:
                await g.driver.execute_query(
                    "MATCH (e:Episodic {uuid:$u}) SET e.salience=$s, e.fixed=$f, e.fade_candidate=$d",
                    u=r["uuid"], s=r["salience"], f=bool(r["salience"] >= fixed_cut),
                    d=bool(r in fade),
                )

        print(f"# Consolidation — {len(rows)} memories  (apply={apply})\n")
        print(f"Top {top} by salience (consolidate / fix):")
        for r in rows[:top]:
            print(f"  {r['salience']:.3f}  {r['name']}  [deg={r['deg']} recall={r['recall']} age={r['age']}d sup={r['sup']}]")
        print(f"\nFade candidates (isolated/old/superseded, never recalled): {len(fade)}")
        for r in fade[:top]:
            print(f"  {r['salience']:.3f}  {r['name']}  [deg={r['deg']} age={r['age']}d sup={r['sup']}] → gated archive proposal")
    finally:
        await g.close()


if __name__ == "__main__":
    asyncio.run(main())
