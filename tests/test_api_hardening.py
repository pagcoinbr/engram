#!/usr/bin/env python3
"""Regression: the local HTTP APIs (bin/engram_api.py + atlas/atlas_api.py).

- every route but the page shell and /login needs the shared token, as a Bearer
  header or the HttpOnly cookie the /login POST form sets (never a URL);
  the token file is created 0600;
- a config save that moves an endpoint's url drops that endpoint's api_key instead
  of sending it to the new host;
- config backups and temp files are created 0600, never at the umask default.

Skips (exit 0) when fastapi is not installed: it is an optional dependency.
"""
import importlib.util
import os
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

try:
    import fastapi  # noqa: F401
    from fastapi.testclient import TestClient
except ImportError:
    print("skip — fastapi not installed")
    sys.exit(0)


def main():
    with tempfile.TemporaryDirectory() as d:
        claude = Path(d) / ".claude"
        claude.mkdir()
        for f in (ROOT / "bin").glob("*.py"):
            shutil.copy(f, claude / f.name)
        config = claude / "engram.yaml"
        config.write_text(
            "backend: llama_cpp\n"
            "llama_cpp: {url: 'http://ai/v1/', model: q, timeout_seconds: 600, api_key: sk-llm}\n"
            "embed: {provider: llama_cpp, url: 'http://e/v1', model: m, dim: 8, api_key: sk-emb}\n"
            "graph: {backend: graphiti_compat}\n")
        config.chmod(0o644)
        os.environ.update(HOME=d, ENGRAM_BIN=str(claude))
        os.environ.pop("ENGRAM_API_TOKEN_FILE", None)
        sys.path.insert(0, str(claude))
        spec = importlib.util.spec_from_file_location("atlas_api", ROOT / "atlas" / "atlas_api.py")
        atlas = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(atlas)
        assert atlas._config_path() == config, atlas._config_path()

        token_file = claude / "engram-api.token"
        assert token_file.stat().st_mode & 0o777 == 0o600, oct(token_file.stat().st_mode)
        token = token_file.read_text().strip()
        assert len(token) == 64 and atlas.base.TOKEN == token

        client = TestClient(atlas.app)
        assert client.get("/api/health").status_code == 401, "no token must be refused"
        bad = client.get("/api/health", headers={"Authorization": "Bearer " + "0" * 64})
        assert bad.status_code == 401, "wrong token must be refused"
        assert client.get("/api/health", headers={"Authorization": f"Bearer {token}"}).status_code == 200
        form = client.get("/login")
        assert form.status_code == 200 and 'method="post"' in form.text
        assert form.headers["cache-control"] == "no-store"
        assert form.headers["referrer-policy"] == "no-referrer"
        assert client.get("/", follow_redirects=False).status_code != 401, "page shell must be public"
        assert client.post("/login", data={"token": "nope"}).status_code == 401
        # a token in the URL is never accepted, even a correct one
        client.get(f"/login?token={token}")
        assert client.get("/api/health").status_code == 401, "query-string token accepted"
        login = client.post("/login", data={"token": token}, follow_redirects=False)
        assert login.status_code == 303, login.status_code
        assert login.headers["cache-control"] == "no-store"
        cookie = login.headers["set-cookie"].lower()
        assert "httponly" in cookie and "samesite=strict" in cookie, cookie
        assert client.get("/api/health").status_code == 200, "login cookie not honoured"
        print("ok — token required (bearer or HttpOnly SameSite=Strict cookie), file 0600")

        def editable(llm_url):
            return {"backend": "llama_cpp", "graph": {"backend": "graphiti_compat"},
                    "llama_cpp": {"url": llm_url, "model": "q", "timeout_seconds": 600},
                    "embed": {"provider": "llama_cpp", "url": "http://e/v1", "model": "m", "dim": 8}}

        atlas._write_config(editable("http://ai/v1"))  # trailing slash only: same endpoint
        saved = config.read_text()
        assert "sk-llm" in saved and "sk-emb" in saved, saved
        atlas._write_config(editable("http://attacker.example/v1"))
        saved = config.read_text()
        assert "sk-llm" not in saved, f"key followed the new host: {saved}"
        assert "sk-emb" in saved, f"unmoved endpoint lost its key: {saved}"
        print("ok — moving an endpoint drops its api_key")

        assert config.stat().st_mode & 0o077 == 0, oct(config.stat().st_mode)
        backups = list((claude / "backups" / "config").glob("*.bak"))
        assert len(backups) == 2, backups
        for b in backups:
            assert b.stat().st_mode & 0o777 == 0o600, (b, oct(b.stat().st_mode))
        print("ok — config and backups are private (0600)")

        # --- Atlas graph view ---------------------------------------------------
        # the graph's project is the engram.env pin, not a hard-coded "-root"
        (claude / "engram.env").write_text('CLAUDE_MEMORY_SLUG="-home-u"\n')
        assert atlas._graph_project() == "-home-u", atlas._graph_project()
        # a store reached through a symlink is listed once, under its real name
        projects = claude / "projects"
        (projects / "-home-u" / "memory").mkdir(parents=True)
        (projects / "-home-u-alias").mkdir(parents=True)
        (projects / "-home-u-alias" / "memory").symlink_to(projects / "-home-u" / "memory")
        atlas.PROJECTS_ROOT = projects
        assert [d.name for d in atlas._project_dirs()] == ["-home-u"], atlas._project_dirs()
        # hub entities (in more than ~2% of memories) draw no shared-entity edge
        nodes = [{"id": f"n{i}"} for i in range(60)]
        ents = {f"n{i}": {"hub"} for i in range(60)}
        ents["n0"] |= {"rare"}
        ents["n1"] |= {"rare"}
        edges = atlas._shared_entity_edges(nodes, ents)
        assert len(edges) == 1 and edges[0]["entities"] == ["rare"], edges
        print("ok — Atlas: pinned graph project, symlinked store listed once, hubs capped")


if __name__ == "__main__":
    main()
