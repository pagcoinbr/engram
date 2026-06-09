"""memory_graph_bootstrap.py — one-time (resumable) seed of the graph from the
canonical .md memory store. Each memory becomes a Graphiti episode (verbatim
body kept; entities + relationship-facts auto-extracted on the local LLM). After
ingest, explicit (:Episodic)-[:LINKS_TO]->(:Episodic) edges are added from
[[wiki-links]] (deterministic, no LLM). Idempotent: a content-hash state file
skips unchanged memories, so the run can be killed and resumed.

Usage:
  python3 memory_graph_bootstrap.py [--limit N] [--only NAME.md ...]
                                    [--rebuild] [--links-only]
"""
import os
import asyncio
import datetime as dt
import hashlib
import json
import logging
import re
import sys
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from graphiti_core.nodes import EpisodeType
from mg_config import build_graphiti, CANONICAL_GROUP

MEM_DIR = Path.home() / ".claude" / "projects" / (os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")) / "memory"
STATE = Path(__file__).resolve().parent / "bootstrap_state.json"
LINK_RE = re.compile(r"\[\[([^\]]+)\]\]")


def parse(md: str):
    meta, body = {}, md
    if md.startswith("---"):
        end = md.find("\n---", 3)
        if end != -1:
            fm = md[3:end]
            for ln in fm.splitlines():
                m = re.match(r"^([A-Za-z_]+):\s*(.*)$", ln)
                if m:
                    meta[m.group(1)] = m.group(2).strip().strip('"')
            if "type" not in meta:  # tolerate nested `metadata:\n  type:`
                mt = re.search(r"^\s+type:\s*(.+)$", fm, re.M)
                if mt:
                    meta["type"] = mt.group(1).strip().strip('"')
            body = md[end + 4:].lstrip("\n")
    return meta, body


def load_state():
    return json.loads(STATE.read_text()) if STATE.exists() else {"done": {}}


def save_state(st):
    STATE.write_text(json.dumps(st, indent=1))


def name_to_uuid(st):
    m = {}
    for name, info in st["done"].items():
        u = info.get("uuid")
        if not u:
            continue
        m[name] = u
        m[name[:-3] if name.endswith(".md") else name] = u   # also index by stem
    return m


async def link_pass(g, st):
    m = name_to_uuid(st)
    total = 0
    for p in sorted(MEM_DIR.glob("*.md")):
        if p.name == "MEMORY.md" or p.name not in st["done"]:
            continue
        _, body = parse(p.read_text(errors="ignore"))
        src = st["done"][p.name]["uuid"]
        for raw in set(LINK_RE.findall(body)):
            t = raw.strip()
            tgt = m.get(t) or m.get(t + ".md") or m.get(t[:-3] if t.endswith(".md") else t)
            if not tgt or tgt == src:
                continue
            try:
                await g.driver.execute_query(
                    "MATCH (a:Episodic {uuid:$s}) MATCH (b:Episodic {uuid:$t}) "
                    "MERGE (a)-[r:LINKS_TO]->(b) RETURN count(r) AS c",
                    s=src, t=tgt,
                )
                total += 1
            except Exception:
                pass
    return total


async def main():
    args = sys.argv[1:]
    limit = int(args[args.index("--limit") + 1]) if "--limit" in args else None
    only = [a for a in args[args.index("--only") + 1:] if not a.startswith("--")] if "--only" in args else []
    rebuild = "--rebuild" in args
    links_only = "--links-only" in args

    files = sorted(p for p in MEM_DIR.glob("*.md") if p.name != "MEMORY.md")
    if only:
        files = [p for p in files if p.name in only]
    if limit:
        files = files[:limit]

    st = {"done": {}} if rebuild else load_state()
    g = build_graphiti()
    await g.build_indices_and_constraints()

    if links_only:
        n = await link_pass(g, st)
        print(f"[bootstrap] link-only pass: {n} LINKS_TO edges merged", flush=True)
        await g.close()
        return

    total = len(files)
    print(f"[bootstrap] {total} memories; {len(st['done'])} already done", flush=True)
    t_start = dt.datetime.now()
    for i, p in enumerate(files, 1):
        raw = p.read_text(errors="ignore")
        h = hashlib.sha256(raw.encode()).hexdigest()[:16]
        if not rebuild and st["done"].get(p.name, {}).get("h") == h:
            print(f"[{i}/{total}] skip (unchanged) {p.name}", flush=True)
            continue
        meta, body = parse(raw)
        ref = dt.datetime.fromtimestamp(p.stat().st_mtime, dt.timezone.utc)
        t0 = dt.datetime.now()
        try:
            res = await g.add_episode(
                name=p.stem, episode_body=body, source=EpisodeType.text,
                source_description=f"canonical memory ({meta.get('type','reference')})",
                reference_time=ref, group_id=CANONICAL_GROUP,
            )
            ep_uuid = getattr(getattr(res, "episode", None), "uuid", None)
            ne = len(getattr(res, "nodes", []) or [])
            ee = len(getattr(res, "edges", []) or [])
            # Store the VERBATIM full .md + frontmatter on the episode node so the
            # graph alone can regenerate the exact file (graph-authoritative model).
            if ep_uuid:
                await g.driver.execute_query(
                    "MATCH (e:Episodic {uuid:$u}) SET e.source_md=$md, "
                    "e.fm_name=$n, e.fm_description=$d, e.fm_type=$t, e.file=$f",
                    u=ep_uuid, md=raw, n=meta.get("name", p.stem),
                    d=meta.get("description", ""), t=meta.get("type", "reference"),
                    f=p.name,
                )
            st["done"][p.name] = {"h": h, "uuid": ep_uuid}
            save_state(st)
            dts = (dt.datetime.now() - t0).total_seconds()
            el = (dt.datetime.now() - t_start).total_seconds()
            print(f"[{i}/{total}] {p.name}: {ne} entities, {ee} facts "
                  f"({dts:.0f}s; elapsed {el/60:.1f}m)", flush=True)
        except Exception as e:
            print(f"[{i}/{total}] ERROR {p.name}: {type(e).__name__}: {str(e)[:160]}", flush=True)

    links = await link_pass(g, st)
    await g.close()
    print(f"[bootstrap] done; {len(st['done'])}/{total} ingested, {links} links, "
          f"in {(dt.datetime.now()-t_start).total_seconds()/60:.1f}m", flush=True)


if __name__ == "__main__":
    asyncio.run(main())
