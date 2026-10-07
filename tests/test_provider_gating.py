#!/usr/bin/env python3
"""Regression: the daemon must only hand work to the Rust binaries they can do.

Two separate ways this went wrong, both silent:

  1. The Rust indexer speaks ONE embedding transport (OpenAI-compatible
     /v1/embeddings). It was preferred whenever the binary merely existed, so an
     Ollama or FastEmbed install — both fully supported engram configurations —
     had its indexing routed to a binary that fails on the URL parse.
  2. The graph backend defaulted differently on the read and write sides: a config
     with no `graph:` block (which is almost every upgraded install, because
     install.sh never adds one) made the daemon write via the native path while
     recall read the Graphiti index, or the reverse once ENGRAM_GRAPH_BACKEND was
     exported — which only the reader honoured.
"""
import importlib.util
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_daemon(home, config_text):
    claude = Path(home) / ".claude"
    claude.mkdir(parents=True, exist_ok=True)
    for f in (ROOT / "bin").glob("*.py"):
        (claude / f.name).write_bytes(f.read_bytes())
    (claude / "engram.yaml").write_text(config_text)
    os.environ["ENGRAM_BIN"] = str(claude)
    os.environ["HOME"] = str(home)
    os.environ.pop("ENGRAM_GRAPH_BACKEND", None)
    os.environ.pop("CLAUDE_MEMORY_SLUG", None)
    sys.path.insert(0, str(claude))
    spec = importlib.util.spec_from_file_location(
        "engram_daemon", ROOT / "daemon" / "engram-daemon.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def write_config(home, text):
    (Path(home) / ".claude" / "engram.yaml").write_text(text)


def test_provider_gating(home):
    mod = load_daemon(home, "backend: ollama\n")
    cases = [
        # (yaml, rust-eligible?, why)
        ("backend: llama_cpp\nembed:\n  provider: llama_cpp\n  url: \"http://127.0.0.1:8091/v1\"\n",
         True, "the one transport Rust implements"),
        ("backend: llama_cpp\nembed:\n  provider: openai\n  url: \"http://e/v1\"\n",
         True, "openai is the same transport"),
        ("backend: llama_cpp\nllama_cpp:\n  url: \"http://e/v1\"\nembed:\n  provider: llama_cpp\n",
         True, "the generation endpoint is a valid fallback for embeddings"),
        ("backend: ollama\nembed:\n  provider: ollama\n", False, "Ollama is Python-only"),
        ("backend: claude\nembed:\n  provider: fastembed\n", False, "FastEmbed is Python-only"),
        ("backend: ollama\n", False, "auto-selected provider resolves to ollama"),
        ("backend: claude\n", False, "auto-selected provider resolves to fastembed"),
        ("backend: llama_cpp\nembed:\n  provider: llama_cpp\n", False, "no endpoint to call"),
    ]
    for yaml_text, want, why in cases:
        write_config(home, yaml_text)
        got = mod.rust_embedding_supported()
        assert got == want, f"{why}: expected rust={want}, got {got} for {yaml_text!r}"
    print(f"ok — Rust indexing claimed only for providers it implements ({len(cases)} configs)")


def test_graph_backend_default_and_override(home):
    mod = load_daemon(home, "backend: ollama\n")

    # No graph block: must stay on the index the install already populated.
    write_config(home, "backend: ollama\n")
    assert mod.graph_backend() == "graphiti_compat", mod.graph_backend()
    assert mod.graphiti_compat_enabled()

    # An explicit choice is honoured.
    write_config(home, "backend: ollama\ngraph:\n  backend: native\n")
    assert mod.graph_backend() == "native"
    assert not mod.graphiti_compat_enabled()

    # The env override must move the WRITER too, not just the reader — otherwise
    # the daemon writes one index while recall reads the other.
    try:
        os.environ["ENGRAM_GRAPH_BACKEND"] = "graphiti_compat"
        assert mod.graph_backend() == "graphiti_compat", \
            "the daemon ignored ENGRAM_GRAPH_BACKEND; reader and writer can diverge"
        assert mod.graphiti_compat_enabled()
        os.environ["ENGRAM_GRAPH_BACKEND"] = "   "
        assert mod.graph_backend() == "native", "an empty export is not a choice"
    finally:
        os.environ.pop("ENGRAM_GRAPH_BACKEND", None)
    print("ok — graph backend: graphiti_compat by default, env override honored on the write side")


def test_native_sync_needs_an_endpoint_for_each_half(home):
    """A native sync embeds AND extracts, through two different services.

    Gating on embeddings alone sent the binary off to fail on an empty generation
    URL. But the requirement is `llama_cpp.url` — what the sync's reasoning client
    is built from — and NOT `backend`, which selects the *Python pipeline's*
    generation backend. A first attempt at this gate required both and declared an
    ordinary `backend: ollama` install with an OpenAI-compatible `llama_cpp.url`
    unserviceable, which it is not.

    Must agree with Config::rust_native_sync_supported in Rust.
    """
    mod = load_daemon(home, "backend: ollama\n")
    embed_ok = "embed:\n  provider: llama_cpp\n  url: \"http://e/v1\"\n"
    generation = "llama_cpp:\n  url: \"http://g/v1\"\n"
    cases = [
        (f"backend: llama_cpp\n{generation}{embed_ok}", True, "both endpoints present"),
        (f"backend: openai\n{generation}{embed_ok}", True, "both endpoints present"),
        (f"backend: claude\n{generation}{embed_ok}", True,
         "`backend` is the Python pipeline's choice; the sync uses llama_cpp.url"),
        (f"backend: ollama\n{generation}{embed_ok}", True,
         "same — an Ollama pipeline alongside an OpenAI-compatible endpoint works"),
        (f"backend: llama_cpp\n{embed_ok}", False, "no generation endpoint configured"),
    ]
    for yaml_text, want, why in cases:
        write_config(home, yaml_text)
        got = mod.rust_native_sync_supported()
        assert got == want, f"{why}: expected {want}, got {got} for {yaml_text!r}"
        # The embedding half is satisfied throughout, which is why one combined
        # check was not enough to catch the missing generation endpoint.
        assert mod.rust_embedding_supported(), why
    print(f"ok — native sync requires an endpoint for each half ({len(cases)} configs)")


def test_native_backend_never_falls_back_to_the_graphiti_writer(home):
    """Writing the index nobody is reading is worse than writing nothing.

    When `graph.backend: native` is selected the reader queries the native index.
    The old fallback ran graph_sync.py — the GRAPHITI writer — whenever the Rust
    sync could not serve the config, so saves landed in one index while recall
    queried the other. That presents as memories that were saved and then cannot
    be recalled, with nothing in the logs connecting the two.
    """
    mod = load_daemon(home, "backend: ollama\n")
    # native selected, but Rust cannot serve it (ollama generation)
    # Unserviceable because there is no generation endpoint for extraction —
    # NOT because of the `backend` value, which the Rust sync does not consult.
    write_config(home, "backend: ollama\ngraph:\n  backend: native\n"
                       "embed:\n  provider: llama_cpp\n  url: \"http://e/v1\"\n")
    assert not mod.graphiti_compat_enabled()
    assert not mod.rust_native_sync_supported()

    ran = []
    mod._run = lambda cmd, **kw: ran.append(cmd) or 0
    mod._neo4j_up = lambda *a, **k: True
    mod._graph_enabled = lambda *a, **k: True
    assert mod.task_graph() is False, "an unserviceable native sync must report failure"
    assert ran == [], f"it wrote to an index nothing is reading: {ran}"
    print("ok — a native backend that Rust cannot serve skips instead of writing Graphiti")


def test_graphiti_maintenance_never_runs_under_a_native_reader(home):
    """Export and reconcile write the LEGACY Graphiti schema.

    `engram-graph-sync` is a wrapper around graph/graph_sync.py, so both branches
    of these jobs wrote Graphiti. The old condition had it backwards — it reached
    for the wrapper precisely when the backend was `native` — so a native reader
    got a nightly Graphiti export, the same split brain task_graph was fixed for.
    Operating on an index nothing reads is worse than not operating: it looks like
    maintenance is happening.
    """
    mod = load_daemon(home, "backend: ollama\n")
    mod._neo4j_up = lambda *a, **k: True
    ran = []
    mod._run = lambda cmd, **kw: ran.append(cmd) or 0

    write_config(home, "backend: ollama\ngraph:\n  backend: native\n")
    assert mod.task_export() is False, "a native backend must not export Graphiti"
    assert mod.task_reconcile() is False, "a native backend must not reconcile Graphiti"
    assert ran == [], f"it ran the legacy Graphiti writer anyway: {ran}"

    # ...and compatibility mode still does both.
    write_config(home, "backend: ollama\ngraph:\n  backend: graphiti_compat\n")
    assert mod.task_export() is True
    assert mod.task_reconcile() is True
    assert len(ran) == 2, ran
    print("ok — Graphiti export/reconcile only run when Graphiti is the reader")


def test_config_path_and_slug_resolution(home):
    """Children must receive the config the daemon actually loaded, and the slug
    pinned by the operator — not re-derived values."""
    mod = load_daemon(home, "backend: ollama\n")
    claude = Path(home) / ".claude"
    assert mod.ENGRAM_CONFIG == claude / "engram.yaml", mod.ENGRAM_CONFIG

    # the operator pin in engram.env must win over a $HOME-derived slug
    assert mod.memory_slug() == str(Path(home)).replace("/", "-")
    (claude / "engram.env").write_text('export CLAUDE_MEMORY_SLUG="-pinned-store"\n')
    assert mod.memory_slug() == "-pinned-store", "engram.env pin ignored"
    os.environ["CLAUDE_MEMORY_SLUG"] = "-env-store"
    try:
        assert mod.memory_slug() == "-env-store", "env must outrank the pin"
    finally:
        os.environ.pop("CLAUDE_MEMORY_SLUG", None)
    print("ok — daemon resolves its own config path and honors the slug pin")


def main():
    with tempfile.TemporaryDirectory() as d:
        test_provider_gating(d)
        test_graph_backend_default_and_override(d)
        test_native_sync_needs_an_endpoint_for_each_half(d)
        test_native_backend_never_falls_back_to_the_graphiti_writer(d)
        test_graphiti_maintenance_never_runs_under_a_native_reader(d)
        test_config_path_and_slug_resolution(d)


if __name__ == "__main__":
    main()
