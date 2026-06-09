"""memory_graph_insert.py — insert Claude-extracted entities/edges into Graphiti's
native schema (so hybrid search + temporal still work), embedding via local nomic.

Reads, for each memory, an extraction JSON produced by Claude:
  {"file":"x.md",
   "entities":[{"name":"server-a","type":"Server","summary":"..."}, ...],
   "edges":[{"source":"worker","relation":"RUNS_ON",
             "target":"api-1","fact":"worker runs on api-1"}, ...]}
from EXTRACT_DIR, plus the canonical .md (for verbatim source_md + body). Entities
merge across memories by canonical name. Episode carries verbatim source_md so the
graph alone regenerates the file. Resumable via a state file.

Usage: python3 memory_graph_insert.py [--only FILE.md ...] [--rebuild]
"""
import asyncio
import datetime as dt
import json
import logging
import os
import re
import sys
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from graphiti_core.nodes import EntityNode, EpisodicNode, EpisodeType
from graphiti_core.edges import EntityEdge
from mg_config import build_graphiti, CANONICAL_GROUP

HERE = Path(__file__).resolve().parent
MEM_DIR = Path.home() / ".claude" / "projects" / (os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")) / "memory"
EXTRACT_DIR = HERE / "extractions"
STATE = HERE / "insert_state.json"
LINK_RE = re.compile(r"\[\[([^\]]+)\]\]")


def parse(md):
    meta, body = {}, md
    if md.startswith("---"):
        end = md.find("\n---", 3)
        if end != -1:
            fm = md[3:end]
            for ln in fm.splitlines():
                m = re.match(r"^([A-Za-z_]+):\s*(.*)$", ln)
                if m:
                    meta[m.group(1)] = m.group(2).strip().strip('"')
            if "type" not in meta:
                mt = re.search(r"^\s+type:\s*(.+)$", fm, re.M)
                if mt:
                    meta["type"] = mt.group(1).strip().strip('"')
            body = md[end + 4:].lstrip("\n")
    return meta, body


def load_state():
    return json.loads(STATE.read_text()) if STATE.exists() else {"done": {}, "entities": {}}


def save_state(st):
    STATE.write_text(json.dumps(st, indent=1))


async def main():
    args = sys.argv[1:]
    rebuild = "--rebuild" in args
    only = [a for a in args[args.index("--only") + 1:] if not a.startswith("--")] if "--only" in args else []

    jsons = sorted(EXTRACT_DIR.glob("*.json"))
    if only:
        jsons = [p for p in jsons if (json.loads(p.read_text()).get("file") in only or p.stem + ".md" in only)]
    if not jsons:
        print(f"no extraction JSONs in {EXTRACT_DIR}"); return

    # global entity type map (first non-empty type wins) for consistent typing
    etype = {}
    for p in jsons:
        for e in json.loads(p.read_text()).get("entities", []):
            nm = e["name"].strip()
            if nm and nm not in etype and e.get("type"):
                etype[nm] = e["type"]

    st = {"done": {}, "entities": {}} if rebuild else load_state()
    ent_uuid = dict(st.get("entities", {}))    # name -> uuid (persisted, merges across runs)

    g = build_graphiti()
    await g.build_indices_and_constraints()
    emb = g.embedder
    now = dt.datetime.now(dt.timezone.utc)

    async def ensure_entity(name):
        name = name.strip()
        if not name:
            return None
        if name in ent_uuid:
            return ent_uuid[name]
        n = EntityNode(name=name, group_id=CANONICAL_GROUP,
                       labels=["Entity", etype.get(name, "Concept")], summary="")
        await n.generate_name_embedding(emb)
        await n.save(g.driver)
        ent_uuid[name] = n.uuid
        return n.uuid

    total = len(jsons)
    ne_tot = ee_tot = 0
    for i, p in enumerate(jsons, 1):
        ex = json.loads(p.read_text())
        fname = ex.get("file") or (p.stem + ".md")
        mdfile = MEM_DIR / fname
        if not mdfile.exists():
            print(f"[{i}/{total}] skip (no .md) {fname}"); continue
        if not rebuild and fname in st["done"]:
            print(f"[{i}/{total}] skip (done) {fname}"); continue
        raw = mdfile.read_text(errors="ignore")
        meta, body = parse(raw)
        ref = dt.datetime.fromtimestamp(mdfile.stat().st_mtime, dt.timezone.utc)

        ep = EpisodicNode(name=mdfile.stem, group_id=CANONICAL_GROUP, source=EpisodeType.text,
                          source_description=f"canonical memory ({meta.get('type','reference')})",
                          content=body, valid_at=ref, created_at=now)
        await ep.save(g.driver)
        await g.driver.execute_query(
            "MATCH (e:Episodic {uuid:$u}) SET e.source_md=$md, e.file=$f, "
            "e.fm_name=$n, e.fm_description=$d, e.fm_type=$t",
            u=ep.uuid, md=raw, f=fname, n=meta.get("name", mdfile.stem),
            d=meta.get("description", ""), t=meta.get("type", "reference"))

        ents = ex.get("entities", [])
        for e in ents:
            await ensure_entity(e["name"])
        nedges = 0
        for ed in ex.get("edges", []):
            s = await ensure_entity(ed.get("source", ""))
            t = await ensure_entity(ed.get("target", ""))
            if not s or not t or s == t:
                continue
            edge = EntityEdge(source_node_uuid=s, target_node_uuid=t,
                              name=ed.get("relation", "RELATES_TO"),
                              fact=ed.get("fact", ""), group_id=CANONICAL_GROUP,
                              episodes=[ep.uuid], created_at=now, valid_at=ref)
            await edge.generate_embedding(emb)
            await edge.save(g.driver)
            nedges += 1
        st["done"][fname] = ep.uuid
        st["entities"] = ent_uuid
        save_state(st)
        ne_tot += len(ents); ee_tot += nedges
        print(f"[{i}/{total}] {fname}: {len(ents)} entities, {nedges} edges", flush=True)

    # [[wiki-link]] episode edges
    name2ep = {f: u for f, u in st["done"].items()}
    stem2ep = {f[:-3]: u for f, u in st["done"].items()}
    links = 0
    for p in jsons:
        fname = json.loads(p.read_text()).get("file") or (p.stem + ".md")
        if fname not in st["done"]:
            continue
        _, body = parse((MEM_DIR / fname).read_text(errors="ignore"))
        src = st["done"][fname]
        for raw_l in set(LINK_RE.findall(body)):
            t = raw_l.strip()
            tgt = name2ep.get(t) or name2ep.get(t + ".md") or stem2ep.get(t) or stem2ep.get(t[:-3] if t.endswith(".md") else t)
            if not tgt or tgt == src:
                continue
            try:
                await g.driver.execute_query(
                    "MATCH (a:Episodic {uuid:$s}) MATCH (b:Episodic {uuid:$t}) "
                    "MERGE (a)-[:LINKS_TO]->(b)", s=src, t=tgt)
                links += 1
            except Exception:
                pass

    await g.close()
    print(f"[insert] {len(st['done'])}/{total} memories, {len(ent_uuid)} unique entities, "
          f"{ee_tot} edges this run, {links} links")


if __name__ == "__main__":
    asyncio.run(main())
