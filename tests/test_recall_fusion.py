#!/usr/bin/env python3
"""Regression: the shared RRF fusion (bin/memory_recall.py) and the prompt hook's
per-session dedup — the two things that decide WHAT reaches the model and HOW OFTEN.

Both run against a synthetic store with no Qdrant/Neo4j/Ollama: the legs drop out,
which is also the fail-open path.
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def mk_store(d, slug):
    mem = Path(d) / ".claude" / "projects" / slug / "memory"
    mem.mkdir(parents=True)
    for i in range(3):
        (mem / f"reference_thing{i}.md").write_text(
            f"---\nname: reference_thing{i}\ndescription: the {i}th thing\n"
            f"metadata:\n  type: reference\n---\nbody about thing {i}\n")
    return mem


def test_fusion(mr):
    cfg = {"recall": {"hybrid": {"k_rrf": 60}}}
    # b.md is the only file two rankers agree on -> RRF must lift it above a.md,
    # which is rank 1 in a single ranker. This is the whole point of fusing.
    rankings = {"graph": ["a.md", "b.md"], "vector": ["b.md", "c.md"], "keyword": ["c.md", "b.md"]}
    names = {"a.md": ("a", "the a"), "b.md": ("b", "the b"), "c.md": ("c", "the c")}
    out = mr.fuse(rankings, names, {"b.md": ["fact one", "fact two"]}, cfg, 3)
    assert [r["file"] for r in out][0] == "b.md", f"RRF did not lift the consensus hit: {out}"
    assert set(out[0]["sources"]) == {"graph", "vector", "keyword"}, out[0]["sources"]
    assert out[0]["facts"] == ["fact one", "fact two"], "facts must survive fusion unsliced"

    # a ranker that returns nothing must not change the ranking (backend down == drops out)
    degraded = mr.fuse({**rankings, "vector": []}, dict(names), {}, cfg, 3)
    assert [r["file"] for r in degraded][0] == "b.md", degraded

    # keyword-only hits carry no metadata -> frontmatter is read from disk
    solo = mr.fuse({"keyword": ["reference_thing1.md"]}, {}, {}, cfg, 3)
    assert solo[0]["description"] == "the 1th thing", solo
    print("ok — RRF fusion (consensus wins, dead ranker drops out, frontmatter filled)")


def test_slug_resolution(mr, home, slug):
    """The store must NOT silently fall back to a slugified $HOME — that bug pointed
    the daemon at an empty store and reported nothing wrong."""
    os.environ.pop("CLAUDE_MEMORY_SLUG", None)
    (Path(home) / ".claude" / "engram.env").write_text(f'CLAUDE_MEMORY_SLUG="{slug}"\n')
    assert mr.resolve_slug() == slug, "engram.env pin ignored"
    os.environ.pop("CLAUDE_MEMORY_SLUG", None)
    (Path(home) / ".claude" / "engram.env").unlink()
    assert mr.resolve_slug("/home/x/proj") == "-home-x-proj", "cwd fallback wrong"
    print("ok — store slug resolution (env > engram.env pin > cwd > $HOME)")


def test_neo4j_transport(mr):
    """The graph leg authenticates with a Basic header — it must never be sent in
    plaintext to a remote host."""
    assert mr._neo4j_base_url("bolt://127.0.0.1:7687") == "http://127.0.0.1:7474"
    assert mr._neo4j_base_url("neo4j+s://graph.example.com:7687") == "https://graph.example.com:7473"
    for bad in ("bolt://graph.example.com:7687", "neo4j://10.0.0.5:7687"):
        try:
            mr._neo4j_base_url(bad)
        except ValueError:
            continue
        raise AssertionError(f"plaintext credentials allowed to remote host: {bad}")
    os.environ["NEO4J_HTTP_URL"] = "http://graph.example.com:7474"
    try:
        mr._neo4j_base_url("bolt://graph.example.com:7687")
        raise AssertionError("plaintext NEO4J_HTTP_URL override accepted for a remote host")
    except ValueError:
        pass
    finally:
        os.environ.pop("NEO4J_HTTP_URL", None)
    print("ok — neo4j transport (loopback http, remote https-only, no silent downgrade)")


def _stub_post(mr, reply):
    """Swap mr._post for a recorder. `reply` is a value to return or an exception
    to raise. Returns (calls, restore)."""
    calls = []

    def post(url, body, headers=None, timeout=5.0):
        calls.append((url, body, headers, timeout))
        if isinstance(reply, BaseException):
            raise reply
        return reply

    old = mr._post
    mr._post = post
    return calls, lambda: setattr(mr, "_post", old)


def test_embedding_provider(mr):
    calls, restore = _stub_post(mr, {"data": [{"embedding": [0.1, 0.2]}]})
    try:
        got = mr.embed("hello", {"embed": {"provider": "llama_cpp",
                                              "url": "http://bge.example/v1",
                                              "model": "bge-m3"}})
    finally:
        restore()
    assert got == [0.1, 0.2], got
    assert calls == [("http://bge.example/v1/embeddings",
                      {"model": "bge-m3", "input": "hello"}, {}, 5.0)], calls
    print("ok — fast recall honors the configured llama.cpp embedding endpoint")


def test_embedding_auth_and_timeout(mr):
    """An authenticated endpoint must get the bearer token, and the caller's budget
    must reach the socket — recall runs on every prompt, so an unbounded embed call
    is a hung prompt."""
    calls, restore = _stub_post(mr, {"data": [{"embedding": [1.0]}]})
    try:
        mr.embed("q", {"embed": {"provider": "openai", "url": "https://api.example/v1",
                                 "model": "m", "api_key": "sk-test"}}, timeout=0.25)
    finally:
        restore()
    url, body, headers, timeout = calls[0]
    assert headers == {"Authorization": "Bearer sk-test"}, headers
    assert timeout == 0.25, timeout
    assert url == "https://api.example/v1/embeddings", url
    print("ok — embed sends the configured api_key and honors the caller's timeout")


def test_embedding_prefixes(mr):
    """Asymmetric models (bge/e5/nomic) index documents and queries in different
    sub-spaces. Applying the wrong prefix — or none — silently degrades recall, so
    kind= must pick the configured one."""
    cfg = {"embed": {"provider": "llama_cpp", "url": "http://e/v1", "model": "m",
                     "query_prefix": "query: ", "document_prefix": "passage: "}}
    for kind, want in (("query", "query: hello"), ("document", "passage: hello")):
        calls, restore = _stub_post(mr, {"data": [{"embedding": [1.0]}]})
        try:
            mr.embed("hello", cfg, kind=kind)
        finally:
            restore()
        assert calls[0][1]["input"] == want, (kind, calls[0][1])
    print("ok — embed applies the configured query/document prefixes")


def test_embedding_failure_propagates(mr):
    """embed() must RAISE, never substitute a vector from another model. The vector
    leg decides what a dead endpoint means; silently returning a 768-dim fastembed
    vector into a 1024-dim index is how an index gets poisoned with no error."""
    _, restore = _stub_post(mr, OSError("connection refused"))
    try:
        try:
            mr.embed("hello", {"embed": {"provider": "llama_cpp", "url": "http://dead/v1"}})
        except OSError:
            pass
        else:
            raise AssertionError("embed() swallowed a dead endpoint")
    finally:
        restore()
    # and the leg above it degrades instead of crashing the prompt
    _, restore = _stub_post(mr, OSError("connection refused"))
    try:
        assert mr.vector_leg("hello", 3, cfg={"vector_store": {"enabled": True},
                                              "embed": {"provider": "llama_cpp",
                                                        "url": "http://dead/v1"}}) == []
    finally:
        restore()
    print("ok — embed raises on failure; vector_leg degrades to empty")


def test_graph_facts_reaches_late_entity(mr):
    """A prose prompt leads with filler words; the entity further in must still be
    looked up. With a 6-token cap this prompt never sent "neo4j" and got 0 facts."""
    sent = {}

    def fake_post(url, body, headers=None, timeout=5.0):
        sent["names"] = body["statements"][0]["parameters"]["names"]
        return {"results": [{"data": [{"row": ["a fact"]}]}]}

    orig = mr._neo4j_http, mr._post
    mr._neo4j_http, mr._post = (lambda: ("http://127.0.0.1:7474/x", "Basic x")), fake_post
    try:
        facts = mr.graph_facts("I'm noticing that when i start a chat qdrant recall and "
                               "neo4j recall isn't working anymore")
    finally:
        mr._neo4j_http, mr._post = orig
    assert "neo4j" in sent["names"], f"late entity dropped by token cap: {sent['names']}"
    assert facts == ["a fact"]
    print("ok — graph facts look up entities past the first few words")


def test_hook_dedup(home, slug):
    """Same prompt twice in one session must inject once; a new session re-injects."""
    hook = ROOT / "bin" / "hooks" / "memory-recall-inject.py"
    env = dict(os.environ, HOME=home, ENGRAM_BIN=str(Path(home) / ".claude"),
               CLAUDE_MEMORY_SLUG=slug, PYTHONPATH=str(ROOT / "bin"))
    payload = json.dumps({"prompt": "tell me everything about the 1th thing please",
                          "session_id": "sess-a"})

    def run(p):
        return subprocess.run([sys.executable, str(hook)], input=p, text=True,
                              capture_output=True, env=env, timeout=60).stdout

    first = run(payload)
    assert "reference_thing1" in first, f"expected a keyword hit, got: {first!r}"
    assert "<relevant-memory" in first and "body about thing" not in first, \
        "must inject descriptions, never memory bodies"
    assert run(payload) == "", "second identical prompt in the same session must be silent"
    other = run(json.dumps({"prompt": "tell me everything about the 1th thing please",
                            "session_id": "sess-b"}))
    assert "reference_thing1" in other, "a NEW session must get the memory again"
    state = json.loads((Path(home) / ".claude" / "logs" / "recall-inject" / "sess-a.json").read_text())
    assert "reference_thing1.md" in state["files"], state
    print("ok — hook dedup (once per session, per-session state, descriptions only)")


def test_hook_gates(home, slug):
    hook = ROOT / "bin" / "hooks" / "memory-recall-inject.py"
    env = dict(os.environ, HOME=home, ENGRAM_BIN=str(Path(home) / ".claude"),
               CLAUDE_MEMORY_SLUG=slug, PYTHONPATH=str(ROOT / "bin"))

    def run(p):
        r = subprocess.run([sys.executable, str(hook)], input=p, text=True,
                           capture_output=True, env=env, timeout=60)
        assert r.returncode == 0, f"hook must never fail the prompt: rc={r.returncode} {r.stderr}"
        return r.stdout

    assert run(json.dumps({"prompt": "hi", "session_id": "g"})) == "", "short prompt not gated"
    assert run(json.dumps({"prompt": "/compact something something", "session_id": "g"})) == "", \
        "slash command not gated"
    assert run("not json at all") == "", "garbage payload must be survived silently"
    print("ok — hook gates (short / slash / garbage all no-op, exit 0)")


def main():
    with tempfile.TemporaryDirectory() as d:
        slug = "-tmp-recall"
        mk_store(d, slug)
        os.environ["HOME"] = d
        os.environ["CLAUDE_MEMORY_SLUG"] = slug
        # engine modules the hook/core import (memory_ai, memory_keyword, ...)
        claude = Path(d) / ".claude"
        for f in (ROOT / "bin").glob("*.py"):
            (claude / f.name).write_bytes(f.read_bytes())
        (claude / "engram.yaml").write_text("local_enabled: true\nvector_store:\n  enabled: false\n")
        sys.path.insert(0, str(claude))
        mr = load("memory_recall", ROOT / "bin" / "memory_recall.py")

        test_fusion(mr)
        test_neo4j_transport(mr)
        test_embedding_provider(mr)
        test_embedding_auth_and_timeout(mr)
        test_embedding_prefixes(mr)
        test_embedding_failure_propagates(mr)
        test_graph_facts_reaches_late_entity(mr)
        test_slug_resolution(mr, d, slug)
        os.environ["CLAUDE_MEMORY_SLUG"] = slug
        test_hook_dedup(d, slug)
        test_hook_gates(d, slug)


if __name__ == "__main__":
    main()
