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


def main():
    with tempfile.TemporaryDirectory() as d:
        os.environ["HOME"] = d
        llm = load_engram_llm(d)
        test_provider_aliases(llm)
        test_explicit_provider_never_falls_back(llm)
        test_auto_provider_still_degrades(llm)
        test_dim_guard(llm)


if __name__ == "__main__":
    main()
