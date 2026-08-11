#!/usr/bin/env python3
"""memory_recall.py — the one hybrid-recall implementation (RRF over graph + vector + keyword).

Three callers share this module so the fusion exists exactly once:
  * hooks/memory-recall-inject.py — the UserPromptSubmit auto-recall hook (--fast)
  * engram-tui.py                 — the Recall tab
  * graph/mg_mcp_server.py        — the memory_recall_hybrid MCP tool (fuse() only)

Two graph strategies, because they have very different costs:
  * --fast  1-hop Neo4j neighbours for entities named in the query (~0.07s).
            What the prompt hook can afford on every prompt.
  * default full graphiti hybrid search via graph/memory_graph_recall.py (seconds,
            cold-loads fastembed). What an interactive recall can afford.

STDLIB ONLY — this module never imports qdrant_client, neo4j, or graphiti. It talks
to Qdrant, Ollama and Neo4j over their HTTP APIs on localhost instead. That is not
purism: `import qdrant_client` measures 0.78s to perform a 0.04s search, and this
runs on every prompt. Whole-module cost is ~0.18s on system python3, which is why
neither the hook nor the TUI needs a venv.

Every leg degrades independently: a disabled vector store, an unreachable Ollama
(no embeddings -> no vector leg), or a down Neo4j just drops out of the fusion. The
keyword (BM25) leg is pure python, so recall never goes completely dark.

CLI:  memory_recall.py "<query>" [--k 6] [--type project] [--cwd DIR] [--fast] [--json]
"""
from __future__ import annotations
import base64
import json
import os
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

HOME = Path.home()
ENGRAM_BIN = Path(os.environ.get("ENGRAM_BIN", HOME / ".claude"))
ENGRAM_GRAPH = Path(os.environ.get("ENGRAM_GRAPH", ENGRAM_BIN / "graph"))
ENGRAM_VECTOR = Path(os.environ.get("ENGRAM_VECTOR", ENGRAM_BIN / "vector"))
GRAPH_PY = os.environ.get("ENGRAM_GRAPH_PYTHON") or str(ENGRAM_GRAPH / "venv" / "bin" / "python")

for _p in (ENGRAM_BIN, ENGRAM_VECTOR):
    if str(_p) not in sys.path:
        sys.path.insert(0, str(_p))

import memory_ai  # noqa: E402

# entity-ish tokens for the fast graph leg (words, dotted names, kebab/snake ids)
TOKEN_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{3,}")


def resolve_slug(cwd: str = "") -> str:
    """Decide WHICH store to search, and pin it in the environment for the legs.

    memory_keyword.slug() and vector_store.slug() both fall back to a slugified
    $HOME, which is an empty store on any box whose memories live under a project
    slug — a recall against it returns nothing and reports no error. Precedence
    mirrors memory_lib.sh: explicit env, then the operator pin in engram.env, then
    the cwd-derived Claude Code project store, then the $HOME default.
    """
    s = os.environ.get("CLAUDE_MEMORY_SLUG")
    if not s:
        envf = ENGRAM_BIN / "engram.env"
        try:
            for line in envf.read_text().splitlines():
                line = line.strip().removeprefix("export ").strip()
                if line.startswith("CLAUDE_MEMORY_SLUG="):
                    s = line.split("=", 1)[1].strip().strip('"').strip("'")
                    break
        except Exception:
            pass
    if not s and cwd:
        s = str(cwd).replace("/", "-")
    if not s:
        s = str(HOME).replace("/", "-")
    os.environ["CLAUDE_MEMORY_SLUG"] = s
    return s


# ---- fusion ---------------------------------------------------------------
def fuse(rankings: dict, names: dict, facts: dict, cfg: dict, k: int) -> list[dict]:
    """RRF-fuse filename-keyed rankings into display records.

    rankings: {ranker: [file, ...]} best-first   names: {file: (name, description)}
    facts:    {file: [fact, ...]}                (graph legs only; may be empty)

    Returns [{"file", "name", "description", "sources", "facts"}] — facts unsliced,
    callers decide how many to show. Pure: no IO beyond frontmatter lookups for
    keyword-only hits, so it is testable in isolation.
    """
    import memory_fusion
    import memory_keyword
    rc = memory_ai.recall_cfg(cfg).get("hybrid", {})
    fused = memory_fusion.fuse(rankings, k_rrf=int(rc.get("k_rrf", 60)),
                               weights=rc.get("weights"))[:k]
    out = []
    for d in fused:
        if d["file"] not in names:              # keyword-only hit -> read frontmatter
            nm, desc, _ = memory_keyword.meta(d["file"])
            names[d["file"]] = (nm, desc)
        nm, desc = names[d["file"]]
        out.append({"file": d["file"], "name": nm or d["file"],
                    "description": (desc or "").strip(), "sources": d["sources"],
                    "facts": facts.get(d["file"], [])})
    return out


def fuse_records(graph_records: list, vector_hits: list, keyword_pairs: list,
                 cfg: dict, k: int) -> list[dict]:
    """fuse() for callers holding the raw leg outputs (graph records / vector hits /
    (file, score) keyword pairs) rather than pre-built rankings."""
    names, facts = {}, {}
    for r in graph_records:
        names.setdefault(r["file"], (r.get("name"), r.get("desc")))
        facts[r["file"]] = r.get("facts", [])
    for h in vector_hits:
        names.setdefault(h["file"], (h.get("name"), h.get("description")))
    rankings = {"graph": [r["file"] for r in graph_records],
                "vector": [h["file"] for h in vector_hits],
                "keyword": [f for f, _ in keyword_pairs]}
    return fuse(rankings, names, facts, cfg, k)


# ---- legs (each returns empty on any failure) ------------------------------
def _post(url: str, body: dict, headers: dict | None = None, timeout: float = 5.0) -> dict:
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json", **(headers or {})})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def embed(text: str, cfg: dict, timeout: float = 5.0) -> list[float]:
    """Embed via Ollama's HTTP API. NOT engram_llm.embed(): that pulls in fastembed
    on non-Ollama backends, and importing it costs more than every round trip here
    combined. A box with no reachable Ollama simply loses the vector leg."""
    host = (cfg.get("ollama") or {}).get("host", "http://localhost:11434")
    model = (cfg.get("expert_models") or {}).get("similarity") or "nomic-embed-text"
    return _post(f"{host.rstrip('/')}/api/embed", {"model": model, "input": text},
                 timeout=timeout)["embeddings"][0]


def vector_leg(query: str, k: int, mtype: str = "", cfg: dict | None = None,
               timeout: float = 5.0) -> list[dict]:
    """Qdrant semantic hits over the REST API — stdlib only, deliberately NOT via
    qdrant_client: importing that library measures 0.78s against a 0.04s search, and
    this runs on every prompt. Same collection, same payload, same slug/type filter
    as vector_store.search(); it is the transport that differs, not the semantics."""
    cfg = cfg or memory_ai.load()
    try:
        if not memory_ai.vector_enabled(cfg):
            return []
        vs = cfg.get("vector_store", {}) or {}
        url = (os.environ.get("ENGRAM_QDRANT_URL") or vs.get("url") or "http://127.0.0.1:6333").rstrip("/")
        coll = os.environ.get("ENGRAM_VECTOR_COLLECTION") or vs.get("collection") or "engram_memory"
        must = []
        if mtype:
            must.append({"key": "type", "match": {"value": mtype}})
        if memory_ai.scope_to_slug(cfg):
            must.append({"key": "slug", "match": {"value": resolve_slug()}})
        body = {"query": embed(query, cfg, timeout), "limit": k, "with_payload": True}
        if must:
            body["filter"] = {"must": must}
        headers = {}
        api_key = os.environ.get("ENGRAM_QDRANT_API_KEY") or vs.get("api_key") or ""
        if api_key:
            headers["api-key"] = api_key
        pts = _post(f"{url}/collections/{coll}/points/query", body, headers, timeout)["result"]["points"]
        return [{"file": (p.get("payload") or {}).get("file"),
                 "name": (p.get("payload") or {}).get("name"),
                 "description": (p.get("payload") or {}).get("description"),
                 "type": (p.get("payload") or {}).get("type"),
                 "score": float(p.get("score") or 0.0)}
                for p in pts if (p.get("payload") or {}).get("file")]
    except Exception:
        return []


def keyword_leg(query: str, k: int, mtype: str = "") -> list[tuple]:
    """BM25 over the .md store — pure python, so effectively always available."""
    try:
        import memory_keyword
        return memory_keyword.rank(query, k, mtype or None)
    except Exception:
        return []


def graph_recall_leg(query: str, k: int, mtype: str = "", timeout: int = 120) -> list[dict]:
    """Full graphiti hybrid search, out-of-process in the graph venv. Seconds, not
    milliseconds — for interactive recall, never for the prompt hook."""
    script = ENGRAM_GRAPH / "memory_graph_recall.py"
    if not script.exists() or not neo4j_up():
        return []
    py = GRAPH_PY if Path(GRAPH_PY).exists() else sys.executable
    try:
        r = subprocess.run([py, str(script), query, "--k", str(k), "--json"],
                           capture_output=True, text=True, timeout=timeout)
        if r.returncode or not r.stdout.strip():
            return []
        records = json.loads(r.stdout)
        return [g for g in records if g.get("type") == mtype] if mtype else records
    except Exception:
        return []


LOOPBACK = {"127.0.0.1", "localhost", "::1", ""}


def _neo4j_base_url(bolt_uri: str) -> str:
    """HTTP base for Neo4j, WITHOUT downgrading the transport.

    This endpoint carries the password in a Basic auth header, so plaintext to a
    remote host would hand the graph to anyone on the path. Rules:
      loopback              -> http://host:7474 (never leaves the machine)
      bolt+s:// / neo4j+s:// -> https://host:7473
      any other remote host  -> refuse; set NEO4J_HTTP_URL explicitly (https)
    """
    from urllib.parse import urlparse
    override = os.environ.get("NEO4J_HTTP_URL")
    bolt = urlparse(bolt_uri)
    host = bolt.hostname or "127.0.0.1"
    if override:
        if urlparse(override).scheme != "https" and (urlparse(override).hostname or "") not in LOOPBACK:
            raise ValueError("NEO4J_HTTP_URL must be https for a non-loopback host")
        return override
    if host in LOOPBACK:
        return f"http://{host or '127.0.0.1'}:7474"
    if bolt.scheme in ("bolt+s", "bolt+ssc", "neo4j+s", "neo4j+ssc"):
        return f"https://{host}:7473"
    raise ValueError(f"refusing to send Neo4j credentials in plaintext to {host}; "
                     "set NEO4J_HTTP_URL to an https endpoint")


def _neo4j_http() -> tuple[str, str]:
    """(transaction endpoint, basic-auth header value). Uses Neo4j's HTTP API so the
    neo4j driver — and mg_config, which drags in graphiti — stay unimported."""
    base = _neo4j_base_url(os.environ.get("NEO4J_URI", "bolt://127.0.0.1:7687"))
    db = os.environ.get("NEO4J_DATABASE", "neo4j")
    user = os.environ.get("NEO4J_USER", "neo4j")
    pw = os.environ.get("NEO4J_PASSWORD", "")
    if not pw:                                  # same source mg_config.neo4j_password() reads
        for line in (ENGRAM_GRAPH / ".env").read_text().splitlines():
            if line.startswith("NEO4J_PASSWORD="):
                pw = line.split("=", 1)[1].strip()
                break
    token = base64.b64encode(f"{user}:{pw}".encode()).decode()
    return f"{base.rstrip('/')}/db/{db}/tx/commit", f"Basic {token}"


def graph_facts(query: str, max_facts: int = 6, max_tokens: int = 6,
                timeout: float = 5.0) -> list[str]:
    """The cheap graph leg: 1-hop RELATES_TO facts for entities named in the query.
    One HTTP round trip for all tokens (UNWIND), so cost is flat in token count."""
    try:
        endpoint, auth = _neo4j_http()
        tokens, seen = [], set()
        for tok in TOKEN_RE.findall(query):
            if tok.lower() in seen:
                continue
            seen.add(tok.lower())
            tokens.append(tok)
            if len(tokens) >= max_tokens:
                break
        if not tokens:
            return []
        res = _post(endpoint, {"statements": [{
            "statement": "UNWIND $names AS nm MATCH (n:Entity)-[r:RELATES_TO]-(m:Entity) "
                         "WHERE toLower(n.name)=toLower(nm) RETURN r.fact AS fact LIMIT $lim",
            "parameters": {"names": tokens, "lim": max_facts}}]},
            {"Authorization": auth}, timeout)
        rows = res["results"][0]["data"]
        return [r["row"][0] for r in rows if r["row"] and r["row"][0]][:max_facts]
    except Exception:
        return []


def neo4j_up() -> bool:
    import socket
    from urllib.parse import urlparse
    p = urlparse(os.environ.get("NEO4J_URI", "bolt://127.0.0.1:7687"))
    try:
        with socket.create_connection((p.hostname or "127.0.0.1", p.port or 7687), timeout=2):
            return True
    except Exception:
        return False


# ---- the whole thing ------------------------------------------------------
def recall(query: str, k: int = 6, mtype: str = "", fast: bool = False,
           cwd: str = "", timeout: float = 5.0) -> dict:
    """Hybrid recall. fast=True swaps the graphiti graph leg for 1-hop neighbour
    facts, which is the difference between ~0.3s and several seconds. `timeout`
    bounds each HTTP leg (the prompt hook lowers it)."""
    resolve_slug(cwd)
    cfg = memory_ai.load()
    want = max(k * 2, 10)
    vector_hits = vector_leg(query, want, mtype, cfg, timeout)
    keyword_pairs = keyword_leg(query, want, mtype)
    graph_records = [] if fast else graph_recall_leg(query, want, mtype)
    facts = graph_facts(query, timeout=timeout) if fast else []
    results = fuse_records(graph_records, vector_hits, keyword_pairs, cfg, k)
    return {"query": query, "results": results, "facts": facts,
            "sources_used": sorted({s for r in results for s in r["sources"]})}


def main():
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        sys.exit(__doc__)
    query, k, mtype, fast, as_json, cwd = args[0], 6, "", False, False, ""
    i = 1
    while i < len(args):
        a = args[i]
        if a == "--k" and i + 1 < len(args):
            k = int(args[i + 1]); i += 1
        elif a == "--type" and i + 1 < len(args):
            mtype = args[i + 1]; i += 1
        elif a == "--cwd" and i + 1 < len(args):
            cwd = args[i + 1]; i += 1
        elif a == "--fast":
            fast = True
        elif a == "--json":
            as_json = True
        i += 1
    out = recall(query, k=k, mtype=mtype, fast=fast, cwd=cwd)
    if as_json:
        print(json.dumps(out))
        return
    if not out["results"]:
        print(f"(no memories matched: {query})")
        return
    print(f"Recalled {len(out['results'])} memories for: {query}\n")
    for r in out["results"]:
        print(f"- {r['name']} [{'+'.join(r['sources'])}]: {r['description']}")
        for f in r["facts"][:2]:
            print(f"    - {f}")
    for f in out["facts"]:
        print(f"  * {f}")


if __name__ == "__main__":
    main()
