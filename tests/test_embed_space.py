#!/usr/bin/env python3
"""Regression: engram_llm.embed() must never cross embedding spaces.

Qdrant and the graph each hold ONE dimension per collection. The old code caught
every exception from the configured provider and fell through to CPU fastembed,
which answers in a different model's 768-dim space — so a llama-server outage on a
1024-dim bge-m3 install quietly wrote mismatched vectors with no error anywhere,
and recall just got worse. An explicitly configured provider is now binding.
"""
import importlib.util
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_engram_llm(home):
    """Import bin/engram_llm.py against a throwaway $HOME (it reads engram.yaml at
    call time via memory_ai, and appends ~/.claude to sys.path at import time)."""
    claude = Path(home) / ".claude"
    claude.mkdir(parents=True, exist_ok=True)
    for f in (ROOT / "bin").glob("*.py"):
        (claude / f.name).write_bytes(f.read_bytes())
    (claude / "engram.yaml").write_text("local_enabled: true\n")
    sys.path.insert(0, str(claude))
    spec = importlib.util.spec_from_file_location("engram_llm", ROOT / "bin" / "engram_llm.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _stub(mod, **fns):
    old = {k: getattr(mod, k) for k in fns}
    for k, v in fns.items():
        setattr(mod, k, v)
    return lambda: [setattr(mod, k, v) for k, v in old.items()]


def boom(*a, **kw):
    raise OSError("connection refused")


def test_explicit_provider_never_falls_back(llm):
    """The whole point: a dead explicit provider is an ERROR, not a quiet downgrade."""
    for provider in ("llama_cpp", "openai", "ollama"):
        cfg = {"embed": {"provider": provider, "url": "http://dead/v1", "dim": 1024}}
        restore = _stub(llm, _llama_embed=boom, _ollama_embed=boom,
                        _fastembed_embed=lambda *a, **k: [0.0] * 768)
        try:
            try:
                llm.embed("hello", cfg)
            except OSError:
                pass
            else:
                raise AssertionError(f"{provider}: embed() fell back instead of failing")
        finally:
            restore()
    print("ok — an explicitly configured provider fails loudly instead of falling back")


def test_auto_provider_still_degrades(llm):
    """With NO provider configured both candidates are 768-dim, so the historical
    fastembed fallback is safe and must stay — it is what makes a GPU-less box work."""
    cfg = {"backend": "ollama", "embed": {}}
    restore = _stub(llm, _ollama_embed=boom, _fastembed_embed=lambda *a, **k: [0.5] * 768)
    try:
        assert llm.embed("hello", cfg) == [0.5] * 768
    finally:
        restore()
    print("ok — the auto-selected default still degrades to fastembed")


def test_dim_guard(llm):
    """A provider that answers in the wrong dimension is caught here, where the cause
    is still visible, rather than later as a rejected upsert or bad recall."""
    cfg = {"embed": {"provider": "llama_cpp", "url": "http://e/v1", "dim": 1024}}
    restore = _stub(llm, _llama_embed=lambda *a, **k: [0.1] * 768)
    try:
        try:
            llm.embed("hello", cfg)
        except RuntimeError as e:
            assert "768" in str(e) and "1024" in str(e), e
        else:
            raise AssertionError("a 768-dim vector was accepted into a 1024-dim space")
    finally:
        restore()

    # the matching dimension passes through untouched
    restore = _stub(llm, _llama_embed=lambda *a, **k: [0.1] * 1024)
    try:
        assert len(llm.embed("hello", cfg)) == 1024
    finally:
        restore()
    print("ok — embed refuses a vector of the wrong dimension")


def test_provider_aliases(llm):
    assert llm._embed_provider({"embed": {"provider": "openai"}}) == "llama_cpp"
    assert llm._embed_provider({"embed": {"provider": "LLAMA_CPP"}}) == "llama_cpp"
    assert llm._embed_provider({"embed": {"provider": "nonsense"}, "backend": "claude"}) == "fastembed"
    assert llm._embed_provider({"embed": {}, "backend": "ollama"}) == "ollama"
    assert not llm._embed_provider_is_explicit({"embed": {"provider": "nonsense"}})
    assert llm._embed_provider_is_explicit({"embed": {"provider": "fastembed"}})
    print("ok — provider resolution: openai aliases to llama_cpp, junk falls to auto")


#: `Config::embedding_space_id` of the reference config below, pinned in Rust as
#: `engram_config::PINNED_EMBEDDING_SPACE_ID`. Both sides assert this literal, so
#: neither can drift without a test failing.
PINNED_EMBEDDING_SPACE_ID = "b941b4f74fc6de19"

REFERENCE_CFG = {"embed": {"provider": "llama_cpp", "url": "http://127.0.0.1:8081/v1",
                           "model": "bge-m3", "dim": 1024}}


def test_space_id_matches_rust(llm):
    """Rust and Python index the SAME Qdrant collection.

    If their fingerprints differed, each would treat the other's records as
    belonging to a foreign space and re-embed the entire store on every run,
    forever. Pinned from both sides against a literal.
    """
    assert llm.embedding_space_id(REFERENCE_CFG) == PINNED_EMBEDDING_SPACE_ID, (
        f"python fingerprint {llm.embedding_space_id(REFERENCE_CFG)} != "
        f"rust {PINNED_EMBEDDING_SPACE_ID}")
    print("ok — the embedding-space fingerprint agrees with the Rust implementation")


def test_space_id_covers_every_vector_affecting_key(llm):
    """Content hashing could not see these changes; the fingerprint must."""
    base = llm.embedding_space_id(REFERENCE_CFG)
    for change in ({"model": "qwen3-embedding-0.6b"},      # same dim, different model
                   {"url": "http://elsewhere/v1"},
                   {"dim": 768},
                   {"query_prefix": "query: "},
                   {"document_prefix": "passage: "}):
        cfg = {"embed": dict(REFERENCE_CFG["embed"], **change)}
        assert llm.embedding_space_id(cfg) != base, f"space unchanged for {change}"
    # and stable for an equivalent config (openai aliases to llama_cpp)
    same = {"embed": dict(REFERENCE_CFG["embed"], provider="openai")}
    assert llm.embedding_space_id(same) == base
    print("ok — the fingerprint moves on model/endpoint/dim/prefix changes only")


def test_prefixes_are_applied_per_side(llm):
    """Both keys were configurable, round-tripped by the editor, and applied by
    nothing — so an asymmetric model indexed and queried in different spaces."""
    cfg = {"embed": {"provider": "llama_cpp", "url": "http://e/v1", "dim": 4,
                     "query_prefix": "query: ", "document_prefix": "passage: "}}
    seen = []
    restore = _stub(llm, _llama_embed=lambda text, c: seen.append(text) or [0.0] * 4)
    try:
        llm.embed("sharks", cfg, kind="document")
        llm.embed("sharks", cfg, kind="query")
        # No kind: NEITHER prefix. Callers that cannot tell the two sides apart
        # (Graphiti's embedder, the reranker, memory_ai.ollama_embed) go through
        # this path, and guessing "document" would move their queries out of the
        # index's space — a wrong prefix is worse than none.
        llm.embed("sharks", cfg)
    finally:
        restore()
    assert seen == ["passage: sharks", "query: sharks", "sharks"], seen
    print("ok — document and query prefixes are applied on their own side only")


def main():
    with tempfile.TemporaryDirectory() as d:
        os.environ["HOME"] = d
        llm = load_engram_llm(d)
        test_provider_aliases(llm)
        test_explicit_provider_never_falls_back(llm)
        test_auto_provider_still_degrades(llm)
        test_dim_guard(llm)
        test_space_id_matches_rust(llm)
        test_space_id_covers_every_vector_affecting_key(llm)
        test_prefixes_are_applied_per_side(llm)


if __name__ == "__main__":
    main()
