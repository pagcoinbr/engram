#!/usr/bin/env python3
"""graph_maint.py — repair jobs the sync path can't do itself.

The .md store is the source of truth; every Episodic node here is derivable from
it, so these deletes are recoverable by re-inserting (at LLM cost, not data loss).

  --dedup            files with >1 Episodic node -> keep the newest, delete the rest.
                     `ep.save()` mints a NEW uuid every run with no MERGE on `file`,
                     so any run that aborted after save but before save_state()
                     duplicates that memory on the next pass.
  --prune-stale      Episodic nodes whose .md no longer exists in the store.
  --refresh-changed  memories whose sha differs from sync_state: delete their episode
                     and drop them from insert_state/sync_state so a normal
                     `graph_sync.py --insert` re-adds them with current content.
                     NOTE: `memory_graph_insert.py --rebuild` does NOT do this — it
                     only resets the local state file and leaves every old node in
                     place, so it DUPLICATES the graph instead of refreshing it.

Dry-run by default; pass --apply to mutate. --all runs the three in a safe order.

Usage: graph_maint.py [--dedup] [--prune-stale] [--refresh-changed] [--all] [--apply]
"""
import asyncio
import hashlib
import json
import logging
import os
import sys
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from mg_config import build_graphiti, CANONICAL_GROUP

HERE = Path(__file__).resolve().parent
SLUG = os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")
MEM_DIR = Path.home() / ".claude" / "projects" / SLUG / "memory"
INSERT_STATE = HERE / "insert_state.json"
SYNC_STATE = HERE / "sync_state.json"
AUDIT = HERE / "graph_maint_audit.jsonl"


def store_files():
    return {p.name: p for p in sorted(MEM_DIR.glob("*.md"))
            if p.name not in ("MEMORY.md", "MEMORY_FULL.md")}


def sha(p: Path):
    return hashlib.sha256(p.read_bytes()).hexdigest()


def audit(action, rows):
    with open(AUDIT, "a") as f:
        for r in rows:
            f.write(json.dumps({"action": action, **r}) + "\n")


async def delete_episodes(g, uuids, apply):
    """Remove episodes and repair the RELATES_TO edges that cite them.

    An edge citing ONLY dead episodes is orphaned prose and is deleted; an edge also
    cited by a surviving episode just loses the dead uuid from its list."""
    if not uuids:
        return (0, 0, 0)
    q_orphan = ("MATCH ()-[r:RELATES_TO]->() "
                "WHERE any(u IN r.episodes WHERE u IN $u) "
                "  AND size([x IN r.episodes WHERE NOT x IN $u]) = 0 "
                "RETURN count(r) AS c")
    q_trim = ("MATCH ()-[r:RELATES_TO]->() "
              "WHERE any(u IN r.episodes WHERE u IN $u) "
              "  AND size([x IN r.episodes WHERE NOT x IN $u]) > 0 "
              "RETURN count(r) AS c")
    orph = (await g.driver.execute_query(q_orphan, u=uuids))[0][0]["c"]
    trim = (await g.driver.execute_query(q_trim, u=uuids))[0][0]["c"]
    if apply:
        await g.driver.execute_query(q_orphan.replace("RETURN count(r) AS c", "DELETE r"), u=uuids)
        await g.driver.execute_query(
            q_trim.replace("RETURN count(r) AS c",
                           "SET r.episodes = [x IN r.episodes WHERE NOT x IN $u]"), u=uuids)
        await g.driver.execute_query(
            "MATCH (e:Episodic) WHERE e.uuid IN $u DETACH DELETE e", u=uuids)
    return (len(uuids), orph, trim)


async def dedup(g, apply):
    r, _, _ = await g.driver.execute_query("""
        MATCH (e:Episodic) WHERE e.file IS NOT NULL AND e.group_id = $grp
        WITH e.file AS f, collect(e) AS es WHERE size(es) > 1
        WITH f, [x IN es | {uuid: x.uuid, created: toString(x.created_at)}] AS all
        RETURN f AS file, all ORDER BY f""", grp=CANONICAL_GROUP)
    # Survivor = the uuid insert_state recorded as done, NOT merely the newest.
    # A duplicate exists precisely because a run re-inserted a memory, and a run can
    # abort after ep.save() but before its edges/state are written — that leaves the
    # NEWEST copy as the incomplete one. insert_state["done"][file] is the uuid of the
    # last insert that ran to completion, so it is the authoritative keep. Fall back
    # to newest only when the state has no opinion, and then correct the state.
    ins = json.loads(INSERT_STATE.read_text()) if INSERT_STATE.exists() else {}
    done = ins.get("done", {})
    dead, kept, restated = [], 0, 0
    for row in r:
        ordered = sorted(row["all"], key=lambda d: (d["created"] or ""), reverse=True)
        recorded = done.get(row["file"])
        if recorded and any(d["uuid"] == recorded for d in ordered):
            survivor = recorded
        else:
            survivor = ordered[0]["uuid"]        # no usable state — newest wins
            if row["file"] in done:
                done[row["file"]] = survivor     # keep state and graph in agreement
                restated += 1
        kept += 1
        losers = [d["uuid"] for d in ordered if d["uuid"] != survivor]
        dead += losers
        audit("dedup", [{"file": row["file"], "keep": survivor,
                         "delete": losers}]) if apply else None
    if apply and restated:
        ins["done"] = done
        tmp = INSERT_STATE.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(ins, indent=1))
        tmp.replace(INSERT_STATE)                # atomic
    n, orph, trim = await delete_episodes(g, dead, apply)
    if restated:
        print(f"[dedup] {restated} file(s) had no usable state uuid — kept newest and "
              f"{'updated' if apply else 'would update'} insert_state to match")
    print(f"[dedup] {len(r)} file(s) duplicated -> keep {kept}, delete {n} episode(s); "
          f"edges: {orph} orphaned-deleted, {trim} trimmed")


async def prune_stale(g, apply):
    have = set(store_files())
    # insert_state/sync_state must lose the pruned files too, or `--status` keeps
    # counting them as "in graph" (589 reported vs 555 real nodes, 2026-08-15).
    ins = json.loads(INSERT_STATE.read_text()) if INSERT_STATE.exists() else {"done": {}}
    syn = json.loads(SYNC_STATE.read_text()) if SYNC_STATE.exists() else {}
    ghosts = [n for n in list(ins.get("done", {})) if n not in have]
    if ghosts:
        print(f"[prune-stale] {len(ghosts)} file(s) in insert_state but not in the store")
        if apply:
            for n in ghosts:
                ins["done"].pop(n, None)
                syn.pop(n, None)
            INSERT_STATE.write_text(json.dumps(ins, indent=1))
            SYNC_STATE.write_text(json.dumps(syn, indent=1))
    r, _, _ = await g.driver.execute_query(
        "MATCH (e:Episodic) WHERE e.file IS NOT NULL AND e.group_id = $grp "
        "RETURN DISTINCT e.file AS f", grp=CANONICAL_GROUP)
    gone = sorted({x["f"] for x in r} - have)
    if gone:
        rr, _, _ = await g.driver.execute_query(
            "MATCH (e:Episodic) WHERE e.file IN $f AND e.group_id = $grp "
            "RETURN e.uuid AS u", f=gone, grp=CANONICAL_GROUP)
        uuids = [x["u"] for x in rr]
        audit("prune-stale", [{"file": f} for f in gone]) if apply else None
        n, orph, trim = await delete_episodes(g, uuids, apply)
        print(f"[prune-stale] {len(gone)} file(s) no longer in store -> delete {n} episode(s); "
              f"edges: {orph} orphaned-deleted, {trim} trimmed")
        print("   " + ", ".join(gone[:8]) + (" ..." if len(gone) > 8 else ""))
    else:
        print("[prune-stale] nothing stale")


async def refresh_changed(g, apply):
    files = store_files()
    sync = json.loads(SYNC_STATE.read_text()) if SYNC_STATE.exists() else {}
    ins = json.loads(INSERT_STATE.read_text()) if INSERT_STATE.exists() else {"done": {}, "entities": {}}
    # Staleness must be decided against what the GRAPH actually holds, not against
    # sync_state: bootstrap-era memories have no sync_state entry, so `.get() != sha`
    # marks all of them changed and would re-extract ~300 current memories for nothing
    # (that is the bogus "293 changed memory(ies)" banner). Episodic.source_md is the
    # verbatim .md at insert time, so comparing it to the file is exact and free.
    rows, _, _ = await g.driver.execute_query(
        "MATCH (e:Episodic) WHERE e.file IS NOT NULL AND e.source_md IS NOT NULL "
        "AND e.group_id = $grp RETURN e.file AS f, e.source_md AS md", grp=CANONICAL_GROUP)
    in_graph = {}
    for x in rows:                       # keep any copy that matches the file
        in_graph.setdefault(x["f"], set()).add(hashlib.sha256(x["md"].encode()).hexdigest())
    changed, fresh = [], []
    for n, p in files.items():
        if n not in ins.get("done", {}) or n not in in_graph:
            continue
        (fresh if sha(p) in in_graph[n] else changed).append(n)
    if fresh and apply:                  # stop the banner lying about these
        for n_ in fresh:
            sync[n_] = sha(files[n_])
        SYNC_STATE.write_text(json.dumps(sync, indent=1))
        print(f"[refresh-changed] {len(fresh)} memory(ies) verified byte-identical to the "
              f"graph — backfilled sync_state, no re-extraction needed")
    elif fresh:
        print(f"[refresh-changed] {len(fresh)} memory(ies) are byte-identical to the graph "
              f"(sync_state would be backfilled, no re-extraction)")
    if not changed:
        print("[refresh-changed] nothing genuinely stale")
        return
    rr, _, _ = await g.driver.execute_query(
        "MATCH (e:Episodic) WHERE e.file IN $f AND e.group_id = $grp "
        "RETURN e.uuid AS u", f=changed, grp=CANONICAL_GROUP)
    uuids = [x["u"] for x in rr]
    # Clear the state BEFORE deleting. Crash ordering matters: state-then-delete can
    # at worst leave a duplicate episode (recoverable with --dedup), whereas
    # delete-then-state leaves the memory gone from the graph but still marked done,
    # so normal sync never restores it and the loss is silent.
    if apply:
        audit("refresh-changed", [{"file": f} for f in changed])
        for n_ in changed:
            ins.get("done", {}).pop(n_, None)
            sync.pop(n_, None)
        for path, data in ((INSERT_STATE, ins), (SYNC_STATE, sync)):
            tmp = path.with_suffix(".json.tmp")
            tmp.write_text(json.dumps(data, indent=1))
            tmp.replace(path)                  # atomic
        print(f"[refresh-changed] cleared {len(changed)} from insert_state/sync_state")
    n, orph, trim = await delete_episodes(g, uuids, apply)
    print(f"[refresh-changed] {len(changed)} memory(ies) with stale content -> delete {n} episode(s); "
          f"edges: {orph} orphaned-deleted, {trim} trimmed")
    if apply:
        print("[refresh-changed] run `graph_sync.py --insert` to re-add with current content")


async def backfill_mentions(g, apply):
    """Create the missing (:Episodic)-[:MENTIONS]->(:Entity) links.

    Pure database work — the entity names are already in extractions/*.json and their
    uuids in insert_state.json, so nothing needs re-extracting (no LLM, no GPU)."""
    ins = json.loads(INSERT_STATE.read_text()) if INSERT_STATE.exists() else {}
    name2uuid = ins.get("entities", {})
    done = ins.get("done", {})
    if not name2uuid:
        print("[backfill-mentions] no entity map in insert_state — nothing to do")
        return
    r, _, _ = await g.driver.execute_query("""
        MATCH (e:Episodic) WHERE e.file IS NOT NULL AND e.group_id = $grp
        OPTIONAL MATCH (e)-[m:MENTIONS]->()
        WITH e.file AS f, e.uuid AS u, count(m) AS c WHERE c = 0
        RETURN f AS file, u AS uuid""", grp=CANONICAL_GROUP)
    todo, linked, missing_json = list(r), 0, 0
    for row in todo:
        jf = HERE / "extractions" / (row["file"][:-3] + ".json")
        if not jf.exists():
            missing_json += 1
            continue
        ex = json.loads(jf.read_text())
        names = set()
        for e in ex.get("entities", []):
            if isinstance(e, dict) and (e.get("name") or "").strip():
                names.add(e["name"].strip())
        for ed in ex.get("edges", []):
            for k in ("source", "target"):
                if (ed.get(k) or "").strip():
                    names.add(ed[k].strip())
        uuids = sorted({name2uuid[n] for n in names if n in name2uuid})
        if not uuids:
            continue
        if apply:
            await g.driver.execute_query(
                "MATCH (ep:Episodic {uuid:$e}) UNWIND $u AS uu "
                "MATCH (n:Entity {uuid:uu}) MERGE (ep)-[:MENTIONS]->(n)",
                e=row["uuid"], u=uuids)
        linked += len(uuids)
    print(f"[backfill-mentions] {len(todo)} episode(s) with no MENTIONS -> "
          f"{linked} link(s){'' if apply else ' would be created'}"
          + (f"; {missing_json} had no extraction JSON" if missing_json else ""))


async def prune_isolated_entities(g, apply):
    """Delete Entity nodes with NO relationships at all — extracted names that never
    landed in an edge. Run AFTER backfill-mentions: that step connects many of them.

    insert_state's name->uuid map MUST lose them too. ensure_entity() trusts that map
    and never checks the node still exists, so a stale mapping makes every later
    memory using that name attach its edges/MENTIONS to a uuid that isn't there —
    silently, with no error. The sweep below also repairs maps damaged by an earlier
    prune that didn't do this."""
    r, _, _ = await g.driver.execute_query(
        "MATCH (n:Entity) WHERE n.group_id = $grp AND NOT (n)-[]-() RETURN n.uuid AS u",
        grp=CANONICAL_GROUP)
    dead = {x["u"] for x in r}
    ins = json.loads(INSERT_STATE.read_text()) if INSERT_STATE.exists() else {}
    ents = ins.get("entities", {})
    # Repair pass: any cached uuid with no node at all (this prune, or a previous one).
    stale = set()
    if ents:
        alive, _, _ = await g.driver.execute_query(
            "MATCH (n:Entity) WHERE n.uuid IN $u RETURN n.uuid AS u", u=sorted(set(ents.values())))
        alive = {x["u"] for x in alive}
        stale = {u for u in ents.values() if u not in alive}
    drop_names = [n for n, u in ents.items() if u in dead or u in stale]
    if apply:
        if dead:
            await g.driver.execute_query(
                "MATCH (n:Entity) WHERE n.uuid IN $u DELETE n", u=sorted(dead))
        if drop_names:
            for n_ in drop_names:
                ents.pop(n_, None)
            ins["entities"] = ents
            tmp = INSERT_STATE.with_suffix(".json.tmp")
            tmp.write_text(json.dumps(ins, indent=1))
            tmp.replace(INSERT_STATE)          # atomic: never a half-written state file
    print(f"[prune-isolated] {len(dead)} entity node(s) with no relationships"
          f"{' deleted' if apply and dead else ' would be deleted' if dead else ''}; "
          f"{len(drop_names)} name->uuid mapping(s) "
          f"{'dropped' if apply else 'would be dropped'} from insert_state"
          + (f" ({len(stale)} were already-missing nodes)" if stale else ""))


async def main():
    a = sys.argv[1:]
    apply = "--apply" in a
    allj = "--all" in a
    if not apply:
        print("*** DRY RUN — nothing will be modified (pass --apply) ***")
    g = build_graphiti()
    try:
        if allj or "--dedup" in a:
            await dedup(g, apply)
        if allj or "--prune-stale" in a:
            await prune_stale(g, apply)
        if allj or "--refresh-changed" in a:
            await refresh_changed(g, apply)
        # Order matters: backfill connects entities that would otherwise look isolated.
        if allj or "--backfill-mentions" in a:
            await backfill_mentions(g, apply)
        if allj or "--prune-isolated" in a:
            await prune_isolated_entities(g, apply)
        r, _, _ = await g.driver.execute_query(
            "MATCH (n:Entity) WHERE n.group_id = $grp AND NOT (n)<-[:MENTIONS]-() "
        "RETURN count(n) AS c", grp=CANONICAL_GROUP)
        print(f"[info] orphan Entity nodes (unmentioned, left in place): {r[0]['c']}")
    finally:
        await g.close()


if __name__ == "__main__":
    asyncio.run(main())
