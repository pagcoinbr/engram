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

# Resolve the engram module path here rather than relying on mg_config to have
# done it: mg_config sets sys.path as a side effect of being imported, so an
# `import engram_tenant` placed above it fails, and placing it below makes
# correctness depend on import ORDER. Both layouts are covered — `bin/` beside
# this directory in the repo, and flat in ~/.claude after install.
_HERE = Path(__file__).resolve().parent
for _candidate in (_HERE.parent / "bin", _HERE.parent, Path.home() / ".claude"):
    if str(_candidate) not in sys.path:
        sys.path.append(str(_candidate))

import engram_tenant
from mg_config import build_graphiti


async def recall_records(query: str, k: int = 8, group: str = None) -> dict:
    """Structured recall: returns {records:[{file,name,desc,facts}], neighbours:[...],
    group} ranked best-first (by fact count, duplicate files collapsed). The shared
    core for both the markdown view (recall) and the GUI's graph leg (--json).

    `group` is the Graphiti group_id to search — one agent identity. It is not
    optional in effect: engram wrote the single literal "canonical" for every
    memory in every store, so an unfiltered search here returned one project's
    facts to another. Defaults to the legacy group so a pre-tenancy install reads
    exactly what it always read.

    The returned `group` is an echo, and callers rely on it. This script parses
    sys.argv by hand and IGNORES unknown flags, so a caller passing --group to an
    older copy would get a silently unfiltered result; the Rust compatibility leg
    refuses a reply whose echo is missing or wrong rather than trusting that the
    flag was understood. Do not remove it from the payload.
    """
    group = group or engram_tenant.LEGACY_GRAPH_GROUP
    g = build_graphiti()
    try:
        # group_ids scopes Graphiti's own hybrid search; the Cypher below scopes
        # the edge -> episode -> file mapping. Both, because the file list is what
        # actually reaches the caller and it must not contain another identity's
        # memories even if the search layer changes behaviour.
        edges = await g.search(query, num_results=k * 3, group_ids=[group])
        fact_by_ep = defaultdict(list)
        for e in edges:
            for u in (getattr(e, "episodes", None) or []):
                fact_by_ep[u].append(e.fact)
        if not fact_by_ep:
            return {"records": [], "neighbours": [], "group": group}

        recs, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic) WHERE e.uuid IN $u AND e.group_id = $g "
            "RETURN e.uuid AS uuid, e.file AS file, e.fm_name AS name, "
            "e.fm_description AS desc, e.fm_type AS type",
            u=list(fact_by_ep.keys()), g=group,
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

        # BOTH ends of the LINKS_TO hop are scoped. Scoping only `e` would let a
        # [[link]] written across identities pull the neighbour's name out of
        # another tenant's graph — a one-hop leak through an edge, which is
        # precisely what a graph is good at.
        nbrs, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic)-[:LINKS_TO]-(n:Episodic) WHERE e.uuid IN $u "
            "AND e.group_id = $g AND n.group_id = $g "
            "RETURN DISTINCT n.fm_name AS name LIMIT 12",
            u=[r for r in ranked][: len(records)], g=group,
        )
        neighbours = sorted({r["name"] for r in nbrs if r["name"]})
        return {"records": records, "neighbours": neighbours, "group": group}
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


async def recall(query: str, k: int = 8, group: str = None) -> str:
    return _format(query, await recall_records(query, k, group))


# Flags that consume the following argument. Needed because the query is
# reconstructed from the leftovers: the previous parser dropped anything starting
# with "--" and then tried to exclude --k's value with `a != str(k)`, so any
# other flag's value was silently appended to the SEARCH QUERY. `--group work`
# searched for "<query> work".
_VALUE_FLAGS = {"--k", "--group"}


def _parse(argv):
    """Split argv into (query, options) without an argparse dependency.

    Hand-rolled on purpose and kept that way: unknown flags must be ignored
    rather than fatal, because engram upgrades the Rust binaries and these
    scripts independently and a newer caller passes flags an older script has
    never heard of. That tolerance is also why `--group` cannot be trusted to
    have been understood — see `recall_records`, which echoes it back.
    """
    query, options, skip = [], {}, False
    for index, arg in enumerate(argv):
        if skip:
            skip = False
            continue
        if arg in _VALUE_FLAGS:
            options[arg] = argv[index + 1] if index + 1 < len(argv) else None
            skip = True
        elif arg.startswith("--"):
            options[arg] = True
        else:
            query.append(arg)
    return " ".join(query), options


async def _main():
    argv = sys.argv[1:]
    if not argv:
        print('usage: memory_graph_recall.py "<query>" [--k N] [--group ID] '
              '[--json|--json-full]')
        return
    query, options = _parse(argv)
    try:
        k = int(options.get("--k") or 8)
    except (TypeError, ValueError):
        k = 8
    group = options.get("--group")
    group = group if isinstance(group, str) and group.strip() else None
    # --json prints ONLY the records, for the existing callers that expect a bare
    # array (memory_recall.graph_recall_leg). --json-full prints the whole reply,
    # including `neighbours` — the 1-hop related memories that --json silently drops
    # and that therefore never reached the hybrid layer or the Rust compatibility
    # path at all — and `group`, the scoping echo the Rust leg verifies.
    data = await recall_records(query, k, group)
    if options.get("--json-full"):
        print(json.dumps(data))
    elif options.get("--json"):
        print(json.dumps(data["records"]))
    else:
        print(_format(query, data))


if __name__ == "__main__":
    asyncio.run(_main())
