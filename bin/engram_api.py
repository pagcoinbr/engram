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
UI_DIR = Path(os.environ.get("ENGRAM_UI_DIR", Path(__file__).resolve().parent.parent / "ui"))
GRAPH_PY = os.environ.get("ENGRAM_GRAPH_PYTHON") or str(ENGRAM_GRAPH / "venv" / "bin" / "python")

sys.path.insert(0, str(ENGRAM_BIN))
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
