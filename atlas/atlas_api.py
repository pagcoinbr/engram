#!/usr/bin/env python3
"""Read-only all-project inventory and evidence-backed graph API for Engram."""
from __future__ import annotations

import hashlib
import importlib.util
import itertools
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ATLAS_ROOT = Path(__file__).resolve().parent
DIST = ATLAS_ROOT / "dist"
ENGRAM_BIN = Path(os.environ.get("ENGRAM_BIN", Path.home() / ".claude"))
PROJECTS_ROOT = Path.home() / ".claude" / "projects"
GRAPH_ROOT = Path(os.environ.get("ENGRAM_GRAPH", ENGRAM_BIN / "graph"))
LINK_RE = re.compile(r"\[\[([^\]]+)\]\]")
SAFE_RE = re.compile(r"^[A-Za-z0-9._-]+$")

os.environ["ENGRAM_UI_DIR"] = str(DIST)
spec = importlib.util.spec_from_file_location("engram_base_api", ENGRAM_BIN / "engram_api.py")
if spec is None or spec.loader is None:
    raise RuntimeError("cannot load Engram API")
base = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = base
spec.loader.exec_module(base)

from fastapi import HTTPException, Query  # noqa: E402
from fastapi.staticfiles import StaticFiles  # noqa: E402

app = base.app
if (DIST / "assets").exists():
    app.mount("/assets", StaticFiles(directory=DIST / "assets"), name="atlas-assets")

_snapshot_cache: dict[str, tuple[float, dict]] = {}
_model_cache: tuple[float, dict] | None = None


def _project_dirs() -> list[Path]:
    if not PROJECTS_ROOT.exists():
        return []
    return sorted(p.parent for p in PROJECTS_ROOT.glob("*/memory") if p.is_dir())


def _project_label(project_id: str) -> str:
    if project_id == "-root":
        return "root"
    return project_id.removeprefix("-").replace("-", " / ")


def _memory_files(project_dir: Path) -> list[Path]:
    store = project_dir / "memory"
    return sorted(p for p in store.glob("*.md") if p.name != "MEMORY.md")


def _safe_project(project_id: str) -> Path:
    if not SAFE_RE.fullmatch(project_id):
        raise HTTPException(400, "invalid project")
    target = PROJECTS_ROOT / project_id
    if target not in _project_dirs():
        raise HTTPException(404, "project not found")
    return target


def _inventory(scope: str) -> tuple[list[dict], list[dict]]:
    projects = []
    nodes = []
    for project_dir in _project_dirs():
        project_id = project_dir.name
        files = _memory_files(project_dir)
        projects.append({"id": project_id, "label": _project_label(project_id), "count": len(files)})
        if scope != "all" and scope != project_id:
            continue
        for path in files:
            raw = path.read_text(errors="ignore")
            meta = base._frontmatter(raw)
            node_id = f"{project_id}:{path.name}"
            nodes.append({
                "id": node_id,
                "project": project_id,
                "projectLabel": _project_label(project_id),
                "file": path.name,
                "name": meta.get("name") or path.stem.replace("_", " "),
                "description": meta.get("description", ""),
                "type": meta.get("type", "reference").lower(),
                "mtime": int(path.stat().st_mtime),
                "links": sorted(set(x.strip() for x in LINK_RE.findall(raw) if x.strip())),
                "indexStatus": "unknown" if project_id != "-root" else "unindexed",
            })
    return projects, nodes


def _graph_evidence(nodes: list[dict]) -> tuple[list[dict], list[str]]:
    warnings = []
    root_by_file = {n["file"]: n for n in nodes if n["project"] == "-root"}
    if not root_by_file:
        return [], warnings
    try:
        episodes = base._graph_query(
            "MATCH (e:Episodic) WHERE e.file IS NOT NULL "
            "RETURN e.uuid AS uuid, e.file AS file, e.valid_at AS valid_at, e.created_at AS created_at")
    except Exception:
        warnings.append("Entity connections are temporarily unavailable.")
        return [], warnings

    episode_to_node = {}
    file_episodes: dict[str, list[str]] = {}
    for ep in episodes:
        fname = ep.get("file")
        if fname in root_by_file:
            episode_to_node[ep["uuid"]] = root_by_file[fname]["id"]
            file_episodes.setdefault(fname, []).append(ep["uuid"])
    for fname, node in root_by_file.items():
        if fname in file_episodes:
            node["indexStatus"] = "indexed"
            node["episodeCount"] = len(file_episodes[fname])

    sync_state_path = GRAPH_ROOT / "sync_state.json"
    try:
        sync_state = json.loads(sync_state_path.read_text())
    except Exception:
        sync_state = {}
    for fname, node in root_by_file.items():
        expected = sync_state.get(fname)
        path = PROJECTS_ROOT / "-root" / "memory" / fname
        if node["indexStatus"] == "indexed" and expected and path.exists():
            actual = hashlib.sha256(path.read_bytes()).hexdigest()
            if actual != expected:
                node["indexStatus"] = "stale"

    ep_ids = list(episode_to_node)
    if not ep_ids:
        return [], warnings
    facts = base._graph_query(
        "MATCH (a:Entity)-[r:RELATES_TO]->(b:Entity) "
        "WHERE any(u IN coalesce(r.episodes, []) WHERE u IN $episodes) "
        "RETURN a.name AS source, labels(a) AS source_labels, r.name AS relation, "
        "r.fact AS fact, b.name AS target, labels(b) AS target_labels, r.episodes AS episodes",
        episodes=ep_ids,
    )
    node_entities: dict[str, set[str]] = {}
    evidence: dict[str, list[dict]] = {}
    for fact in facts:
        for ep in fact.get("episodes") or []:
            node_id = episode_to_node.get(ep)
            if not node_id:
                continue
            ents = node_entities.setdefault(node_id, set())
            ents.update(x for x in (fact.get("source"), fact.get("target")) if x)
            evidence.setdefault(node_id, []).append({
                "source": fact.get("source"), "relation": fact.get("relation"),
                "target": fact.get("target"), "fact": fact.get("fact", ""),
            })
    for node in nodes:
        node["entityCount"] = len(node_entities.get(node["id"], set()))

    shared_edges = []
    connected = [(node_id, ents) for node_id, ents in node_entities.items() if ents]
    for (left, left_entities), (right, right_entities) in itertools.combinations(connected, 2):
        shared = sorted(left_entities & right_entities, key=str.casefold)
        if not shared:
            continue
        edge_id = "shared:" + hashlib.sha1(f"{left}|{right}".encode()).hexdigest()[:16]
        shared_edges.append({
            "id": edge_id, "source": left, "target": right, "kind": "shared_entity",
            "count": len(shared), "entities": shared[:12], "truncated": len(shared) > 12,
        })
    return shared_edges, warnings


def _wiki_edges(nodes: list[dict]) -> list[dict]:
    by_project_file = {(n["project"], n["file"]): n for n in nodes}
    by_file: dict[str, list[dict]] = {}
    for node in nodes:
        by_file.setdefault(node["file"], []).append(node)
        by_file.setdefault(Path(node["file"]).stem, []).append(node)
    edges = []
    seen = set()
    for node in nodes:
        for raw_target in node.pop("links", []):
            target_file = raw_target if raw_target.endswith(".md") else raw_target + ".md"
            target = by_project_file.get((node["project"], target_file))
            if not target:
                matches = by_file.get(raw_target, []) or by_file.get(target_file, [])
                target = matches[0] if len(matches) == 1 else None
            if not target or target["id"] == node["id"]:
                continue
            key = (node["id"], target["id"])
            if key in seen:
                continue
            seen.add(key)
            edges.append({
                "id": "wiki:" + hashlib.sha1("|".join(key).encode()).hexdigest()[:16],
                "source": node["id"], "target": target["id"], "kind": "wiki_link", "count": 1,
            })
    return edges


def _build_snapshot(scope: str) -> dict:
    projects, nodes = _inventory(scope)
    wiki = _wiki_edges(nodes)
    shared, warnings = _graph_evidence(nodes)
    edges = wiki + shared
    counts = {"indexed": 0, "unindexed": 0, "stale": 0, "unknown": 0}
    for node in nodes:
        counts[node["indexStatus"]] = counts.get(node["indexStatus"], 0) + 1
    revision_payload = [(n["id"], n["mtime"], n["indexStatus"]) for n in nodes]
    revision_payload += [(e["id"], e["count"]) for e in edges]
    revision = hashlib.sha256(json.dumps(revision_payload, sort_keys=True).encode()).hexdigest()[:16]
    return {
        "revision": revision,
        "scope": scope,
        "coverage": {"totalMemories": len(nodes), **counts},
        "projects": projects,
        "nodes": nodes,
        "edges": edges,
        "warnings": warnings,
        "complete": True,
        "generatedAt": int(time.time()),
    }


def _redact(value, key: str = ""):
    if any(term in key.lower() for term in ("key", "token", "password", "secret")):
        return "••••••••" if value else ""
    if isinstance(value, dict):
        return {name: _redact(item, name) for name, item in value.items()}
    if isinstance(value, list):
        return [_redact(item) for item in value]
    return value


def _request_json(url: str, body: dict | None = None, timeout: float = 3.0) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read().decode())


def _models_status() -> dict:
    cfg = base.memory_ai.load()
    embed = cfg.get("embed", {}) or {}
    llama = cfg.get("llama_cpp", {}) or {}
    backend = cfg.get("backend", "ollama")
    models = []
    for role, provider, endpoint, model, dimension in (
        ("reasoning", backend, llama.get("url", ""), llama.get("model", ""), None),
        ("embedding", embed.get("provider", ""), embed.get("url", ""), embed.get("model", ""), embed.get("dim")),
    ):
        item = {"role": role, "provider": provider or "auto", "endpoint": endpoint,
                "configuredModel": model or "auto", "expectedDimension": dimension,
                "reachable": None, "observedModel": "", "observedDimension": None, "latencyMs": None, "error": ""}
        if provider not in ("llama_cpp", "openai") or not endpoint:
            models.append(item)
            continue
        started = time.monotonic()
        try:
            discovered = _request_json(endpoint.rstrip("/") + "/models")
            ids = [entry.get("id", "") for entry in discovered.get("data", [])]
            item["reachable"] = True
            item["observedModel"] = ids[0] if ids else "server reachable"
            if role == "embedding":
                probe = _request_json(endpoint.rstrip("/") + "/embeddings", {"model": model, "input": "engram health probe"})
                item["observedDimension"] = len(probe.get("data", [{}])[0].get("embedding", []))
            item["latencyMs"] = round((time.monotonic() - started) * 1000)
        except (OSError, ValueError, KeyError, IndexError, urllib.error.URLError) as exc:
            item["reachable"] = False
            item["error"] = str(exc)[:180]
        models.append(item)
    return {"models": models, "generatedAt": int(time.time())}


@app.get("/api/atlas/projects")
def atlas_projects():
    projects, _ = _inventory("all")
    return {"projects": projects, "total": sum(x["count"] for x in projects)}


@app.get("/api/atlas/snapshot")
def atlas_snapshot(project: str = Query("all")):
    if project != "all":
        _safe_project(project)
    cached = _snapshot_cache.get(project)
    if cached and time.monotonic() - cached[0] < 15:
        return cached[1]
    snapshot = _build_snapshot(project)
    _snapshot_cache[project] = (time.monotonic(), snapshot)
    return snapshot


@app.get("/api/atlas/memory")
def atlas_memory(project: str, file: str):
    project_dir = _safe_project(project)
    if not base.NAME_RE.fullmatch(file):
        raise HTTPException(400, "invalid filename")
    path = project_dir / "memory" / file
    if not path.is_file():
        raise HTTPException(404, "memory not found")
    raw = path.read_text(errors="ignore")
    meta = base._frontmatter(raw)
    return {"project": project, "file": file, "metadata": meta, "content": raw}


@app.get("/api/atlas/models")
def atlas_models():
    global _model_cache
    if _model_cache and time.monotonic() - _model_cache[0] < 15:
        return _model_cache[1]
    _model_cache = (time.monotonic(), _models_status())
    return _model_cache[1]


@app.get("/api/atlas/config")
def atlas_config():
    return {"path": str(base.memory_ai.CONFIG_PATH), "config": _redact(base.memory_ai.load()), "readOnly": True}


def main():
    import uvicorn
    host = os.environ.get("ENGRAM_UI_HOST", "127.0.0.1")
    port = int(os.environ.get("ENGRAM_UI_PORT", "8765"))
    uvicorn.run(app, host=host, port=port, log_level="warning")


if __name__ == "__main__":
    main()
