#!/usr/bin/env python3
"""engram_api.py — local-first FastAPI backend for the engram GUI.

Binds 127.0.0.1 only — your memories never leave the machine. It is a *view* over
the engine: every mutation calls the same gated scripts the CLI uses
(save_memory.sh / delete_memory.sh), so the GUI cannot bypass the safety model.

Run it via `engram-ui.sh` (which picks the graph venv python so graph endpoints
work). Endpoints under /api/*; the SPA (ui/index.html) is served at /.
"""
from __future__ import annotations
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HOME = Path.home()
ENGRAM_BIN = Path(os.environ.get("ENGRAM_BIN", HOME / ".claude"))
ENGRAM_GRAPH = Path(os.environ.get("ENGRAM_GRAPH", ENGRAM_BIN / "graph"))
ENGRAM_VECTOR = Path(os.environ.get("ENGRAM_VECTOR", ENGRAM_BIN / "vector"))
UI_DIR = Path(os.environ.get("ENGRAM_UI_DIR", Path(__file__).resolve().parent.parent / "ui"))
GRAPH_PY = os.environ.get("ENGRAM_GRAPH_PYTHON") or str(ENGRAM_GRAPH / "venv" / "bin" / "python")

sys.path.insert(0, str(ENGRAM_BIN))
# The optional vector store modules live in ~/.claude/vector — make them importable
# so the API can query Qdrant in-process (it runs under the graph venv, which has
# qdrant-client when installed with --vector).
if str(ENGRAM_VECTOR) not in sys.path:
    sys.path.insert(0, str(ENGRAM_VECTOR))
import memory_ai  # noqa: E402

try:
    from fastapi import FastAPI, HTTPException, Body
    from fastapi.responses import FileResponse, JSONResponse, PlainTextResponse
except ImportError:
    sys.exit("fastapi not installed — `pip install fastapi uvicorn` (the installer puts it in the graph venv)")

NAME_RE = re.compile(r"^[A-Za-z0-9._-]+\.md$")   # path-traversal guard


def _slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")

def STORE() -> Path:
    return HOME / ".claude" / "projects" / _slug() / "memory"

def _frontmatter(text: str) -> dict:
    meta = {}
    if text.startswith("---"):
        end = text.find("\n---", 3)
        if end != -1:
            for ln in text[3:end].splitlines():
                m = re.match(r"^([A-Za-z_]+):\s*(.*)$", ln)
                if m:
                    meta[m.group(1)] = m.group(2).strip().strip('"')
                elif re.match(r"^\s+type:\s*", ln):  # nested metadata.type
                    meta["type"] = ln.split("type:", 1)[1].strip().strip('"')
    return meta

def _safe(fname: str) -> str:
    if not NAME_RE.match(fname or ""):
        raise HTTPException(400, "invalid filename")
    return fname

def _engine_python() -> str:
    return sys.executable


# ---- optional vector store helpers (degrade gracefully when off/unreachable) ----
def _vector_python() -> str:
    """Python that can run vector_sync.py (needs qdrant-client): the vector venv if
    present, the ENGRAM_VECTOR_PYTHON override, else this interpreter."""
    cand = os.environ.get("ENGRAM_VECTOR_PYTHON") or str(ENGRAM_VECTOR / "venv" / "bin" / "python")
    return cand if Path(cand).exists() else sys.executable

def _vector_filters(mtype: str = ""):
    """Build a Qdrant payload filter from a `type` arg + the default slug scope."""
    cfg = memory_ai.load()
    f = {}
    if mtype:
        f["type"] = mtype
    if memory_ai.scope_to_slug(cfg):
        try:
            from vector_store import slug
            f["slug"] = slug()
        except Exception:
            pass
    return f or None

def _vector_store():
    """Build an EngramVectorStore, or raise (VectorUnavailable / ImportError / conn
    error). Callers catch and return a graceful notice."""
    from vector_config import VectorUnavailable
    cfg = memory_ai.load()
    if not memory_ai.vector_enabled(cfg):
        raise VectorUnavailable("vector_store disabled (or local_enabled false)")
    from vector_store import EngramVectorStore
    s = EngramVectorStore(cfg)
    s.ensure_collection()
    return s

app = FastAPI(title="engram", docs_url=None, redoc_url=None)


@app.get("/api/health")
def health():
    out = {"backend": None, "neo4j": False, "daemon": None, "store": str(STORE()), "memories": 0}
    try:
        import engram_llm
        out.update(engram_llm.health(memory_ai.load()))
    except Exception as e:
        out["error"] = str(e)
    out["neo4j"] = _neo4j_up()
    st = ENGRAM_BIN / "logs" / "daemon_state.json"
    if not st.exists():
        st = HOME / ".claude" / "logs" / "daemon_state.json"
    try:
        out["daemon"] = json.loads(st.read_text())
    except Exception:
        out["daemon"] = None
    try:
        out["memories"] = len([p for p in STORE().glob("*.md") if p.name != "MEMORY.md"])
    except Exception:
        pass
    # optional vector store summary (for the Dashboard card)
    cfg = memory_ai.load()
    if not memory_ai.vector_enabled(cfg):
        out["vector"] = {"enabled": False}
    else:
        try:
            out["vector"] = {"enabled": True, "reachable": True, "points": _vector_store().stats()["points"]}
        except Exception as e:
            out["vector"] = {"enabled": True, "reachable": False, "error": str(e)[:200]}
    return out


@app.get("/api/memories")
def list_memories():
    store = STORE()
    items = []
    if store.exists():
        for p in sorted(store.glob("*.md")):
            if p.name == "MEMORY.md":
                continue
            fm = _frontmatter(p.read_text(errors="ignore"))
            items.append({"file": p.name, "name": fm.get("name", p.stem),
                          "description": fm.get("description", ""), "type": fm.get("type", "reference")})
    return {"memories": items}


@app.get("/api/memories/{fname}")
def get_memory(fname: str):
    p = STORE() / _safe(fname)
    if not p.exists():
        raise HTTPException(404, "not found")
    return {"file": fname, "content": p.read_text(errors="ignore")}


@app.post("/api/memories")
def save_memory(payload: dict = Body(...)):
    fname = _safe(payload.get("filename", ""))
    desc = (payload.get("description") or "").strip() or "(no description)"
    content = payload.get("content") or ""
    if not content.strip():
        raise HTTPException(400, "empty content")
    r = subprocess.run(["bash", str(ENGRAM_BIN / "save_memory.sh"), fname, desc],
                       input=content, text=True, capture_output=True)
    if r.returncode:
        raise HTTPException(500, (r.stderr or "save failed")[-400:])
    return {"ok": True, "file": fname, "out": r.stdout.strip()}


@app.delete("/api/memories/{fname}")
def delete_memory(fname: str):
    fname = _safe(fname)
    script = ENGRAM_BIN / "delete_memory.sh"
    if not script.exists():
        raise HTTPException(501, "delete_memory.sh not installed")
    r = subprocess.run(["bash", str(script), fname], capture_output=True, text=True)
    if r.returncode:
        raise HTTPException(500, (r.stderr or "delete failed")[-400:])
    return {"ok": True}


@app.get("/api/scores")
def scores():
    sp = ENGRAM_BIN / "memory_score.py"
    if not sp.exists():
        return {"scores": [], "note": "memory_score.py not installed"}
    r = subprocess.run([_engine_python(), str(sp), "--json"], capture_output=True, text=True, timeout=120)
    if r.returncode:
        return JSONResponse({"error": (r.stderr or "")[-400:]}, status_code=500)
    try:
        return {"scores": json.loads(r.stdout)}
    except Exception:
        return {"scores": [], "raw": r.stdout[-2000:]}


@app.get("/api/queues")
def queues():
    store = STORE()
    def listing(area):
        d = store / area
        return [p.name for p in sorted(d.glob("*.md"))] if d.exists() else []
    return {"staging": listing(".staging"), "quarantine": listing(".quarantine")}


@app.get("/api/skills")
def skills():
    base = HOME / ".claude" / "skills"
    out = {"installed": [], "pending": []}
    if base.exists():
        for d in sorted(base.iterdir()):
            if not d.is_dir() or d.name.startswith("."):
                continue
            sk = d / "SKILL.md"
            desc = ""
            if sk.exists():
                m = re.search(r"(?m)^description:\s*(.+)$", sk.read_text(errors="ignore"))
                desc = (m.group(1).strip().strip(">").strip() if m else "")[:300]
            out["installed"].append({"name": d.name, "description": desc})
        pend = base / ".pending"
        if pend.exists():
            out["pending"] = [p.name for p in sorted(pend.glob("*"))]
    return out


@app.get("/api/config")
def get_config():
    return memory_ai.load()


# ---- vector store (optional Qdrant index) ---------------------------------
@app.get("/api/vector/stats")
def vector_stats():
    cfg = memory_ai.load()
    if not memory_ai.vector_enabled(cfg):
        return {"enabled": False}
    try:
        return {"enabled": True, "reachable": True, **_vector_store().stats()}
    except Exception as e:
        return {"enabled": True, "reachable": False, "error": str(e)[:200]}


@app.get("/api/vector/search")
def vector_search(q: str, k: int = 8, type: str = ""):
    try:
        hits = _vector_store().search(q, k=k, filters=_vector_filters(type))
        return {"hits": hits}
    except Exception as e:
        return {"hits": [], "note": f"vector store unavailable: {str(e)[:200]}"}


@app.post("/api/vector/sync")
def vector_sync(payload: dict = Body(...)):
    mode = (payload.get("mode") or "insert").strip()
    if mode not in ("insert", "rebuild"):
        raise HTTPException(400, "mode must be 'insert' or 'rebuild'")
    script = ENGRAM_VECTOR / "vector_sync.py"
    if not script.exists():
        raise HTTPException(501, "vector_sync.py not installed (run ./install.sh --vector)")
    flag = "--rebuild" if mode == "rebuild" else "--insert"
    r = subprocess.run([_vector_python(), str(script), flag],
                       capture_output=True, text=True, timeout=1800)
    if r.returncode:
        raise HTTPException(500, (r.stderr or "sync failed")[-400:])
    return {"ok": True, "mode": mode, "out": (r.stdout or "").strip()[-2000:]}


# ---- hybrid recall (RRF over graph + vector + keyword) --------------------
def _hybrid_fuse(graph_records, vector_hits, keyword_pairs, cfg, k):
    """Pure fusion core (no FastAPI/IO) — testable in isolation. Fuses three
    filename-keyed rankings via RRF and assembles display records."""
    import memory_fusion
    import memory_keyword
    rc = memory_ai.recall_cfg(cfg).get("hybrid", {})
    rankings = {"graph": [r["file"] for r in graph_records],
                "vector": [h["file"] for h in vector_hits],
                "keyword": [f for f, _ in keyword_pairs]}
    names, facts = {}, {}
    for r in graph_records:
        names.setdefault(r["file"], (r.get("name"), r.get("desc")))
        facts[r["file"]] = r.get("facts", [])
    for h in vector_hits:
        names.setdefault(h["file"], (h.get("name"), h.get("description")))
    fused = memory_fusion.fuse(rankings, k_rrf=int(rc.get("k_rrf", 60)),
                               weights=rc.get("weights"))[:k]
    results = []
    for d in fused:
        if d["file"] not in names:                  # keyword-only hit -> frontmatter
            nm, desc, _ = memory_keyword.meta(d["file"])
            names[d["file"]] = (nm, desc)
        nm, desc = names[d["file"]]
        results.append({"file": d["file"], "name": nm or d["file"],
                        "description": desc or "", "sources": d["sources"],
                        "facts": facts.get(d["file"], [])[:3]})
    return results


@app.get("/api/recall/hybrid")
def recall_hybrid(q: str, k: int = 6, type: str = ""):
    cfg = memory_ai.load()
    want = max(k * 2, 10)
    # vector leg (in-process; optional)
    vector_hits = []
    try:
        if memory_ai.vector_enabled(cfg):
            vector_hits = _vector_store().search(q, k=want, filters=_vector_filters(type))
    except Exception:
        pass
    # keyword leg (pure-python; effectively always available)
    try:
        import memory_keyword
        keyword_pairs = memory_keyword.rank(q, want, type or None)
    except Exception:
        keyword_pairs = []
    # graph leg (subprocess to the graph venv — decoupled, same as /api/graph/recall)
    graph_records = []
    script = ENGRAM_GRAPH / "memory_graph_recall.py"
    if script.exists() and _neo4j_up():
        py = GRAPH_PY if Path(GRAPH_PY).exists() else _engine_python()
        try:
            r = subprocess.run([py, str(script), q, "--k", str(want), "--json"],
                               capture_output=True, text=True, timeout=120)
            if r.returncode == 0 and r.stdout.strip():
                graph_records = json.loads(r.stdout)
                if type:                            # graph leg type-filter (records carry type)
                    graph_records = [g for g in graph_records if g.get("type") == type]
        except Exception:
            graph_records = []
    results = _hybrid_fuse(graph_records, vector_hits, keyword_pairs, cfg, k)
    sources = sorted({s for r in results for s in r["sources"]})
    return {"query": q, "results": results, "sources_used": sources}


# ---- graph (degrades gracefully if the venv/Neo4j is absent) --------------
def _neo4j_up() -> bool:
    import socket
    from urllib.parse import urlparse
    p = urlparse(os.environ.get("NEO4J_URI", "bolt://127.0.0.1:7687"))
    try:
        with socket.create_connection((p.hostname or "127.0.0.1", p.port or 7687), timeout=2):
            return True
    except Exception:
        return False


@app.get("/api/graph/recall")
def graph_recall(q: str, k: int = 6):
    script = ENGRAM_GRAPH / "memory_graph_recall.py"
    if not script.exists() or not _neo4j_up():
        return PlainTextResponse("(graph unavailable — Neo4j down or graph not installed)")
    py = GRAPH_PY if Path(GRAPH_PY).exists() else _engine_python()
    r = subprocess.run([py, str(script), q, "--k", str(k)], capture_output=True, text=True, timeout=120)
    return PlainTextResponse(r.stdout or (r.stderr or "(no output)")[-400:])


def _graph_query(cypher, **params):
    try:
        import neo4j  # only present under the graph venv
        sys.path.insert(0, str(ENGRAM_GRAPH))
        import mg_config
        drv = neo4j.GraphDatabase.driver(os.environ.get("NEO4J_URI", "bolt://127.0.0.1:7687"),
                                          auth=(os.environ.get("NEO4J_USER", "neo4j"), mg_config.neo4j_password()))
        with drv.session() as s:
            return [dict(r) for r in s.run(cypher, **params)]
    except Exception as e:
        raise HTTPException(503, f"graph unavailable: {e}")


@app.get("/api/graph/stats")
def graph_stats():
    rows = _graph_query(
        "MATCH (e:Episodic) WITH count(e) AS eps MATCH (n:Entity) WITH eps, count(n) AS ents "
        "OPTIONAL MATCH ()-[r:RELATES_TO]->() RETURN eps AS episodes, ents AS entities, count(r) AS facts")
    return rows[0] if rows else {"episodes": 0, "entities": 0, "facts": 0}


@app.get("/api/graph/neighbors")
def graph_neighbors(entity: str):
    rows = _graph_query(
        "MATCH (n:Entity)-[r:RELATES_TO]-(m:Entity) WHERE toLower(n.name)=toLower($e) "
        "RETURN r.name AS rel, m.name AS other, r.fact AS fact LIMIT 50", e=entity)
    return {"entity": entity, "edges": rows}


# ---- serve the SPA --------------------------------------------------------
@app.get("/")
def index():
    idx = UI_DIR / "index.html"
    if idx.exists():
        return FileResponse(str(idx))
    return PlainTextResponse("engram API is up. UI not found at " + str(idx))


def main():
    import uvicorn
    host = os.environ.get("ENGRAM_UI_HOST", "127.0.0.1")
    port = int(os.environ.get("ENGRAM_UI_PORT", "8765"))
    print(f"engram GUI: http://{host}:{port}  (store: {STORE()})", flush=True)
    uvicorn.run(app, host=host, port=port, log_level="warning")


if __name__ == "__main__":
    main()
