#!/usr/bin/env python3
"""graph/mg_config.py must resolve its extraction model through the normal
experts > tier-preset chain. It used to hardcode "llama3.1:8b" (a frozen copy of
the tier-small preset), so an operator who pinned `experts:` to their own model
got a 404 on every extraction call."""
import sys, types, importlib.util, os
from pathlib import Path

MG = Path(__file__).resolve().parent.parent / "graph" / "mg_config.py"


def _load(experts, env):
    for k in ("MG_LLM_MODEL", "MG_SMALL_MODEL", "MG_REASONING_EFFORT"):
        os.environ.pop(k, None)
    os.environ.update(env)

    mai = types.ModuleType("memory_ai")
    mai.load = lambda: {"experts": experts, "tier": "small"}
    sys.modules["memory_ai"] = mai

    ell = types.ModuleType("engram_llm")
    def model_for(role, cfg=None):
        e = experts.get(role)
        if isinstance(e, dict) and e.get("model"):
            return e["model"]
        return {"distill": "llama3.1:8b", "triage": "llama3.2:3b"}[role]
    ell.model_for = model_for
    sys.modules["engram_llm"] = ell

    # graphiti + neo4j imports are heavy and irrelevant here; exec only the model
    # resolution block and read the constants the bootstrap actually consumes.
    src = MG.read_text().split("OLLAMA_BASE", 1)[1].split("def _neo4j_uri")[0]
    src = "OLLAMA_BASE" + src
    ns = {"os": os, "engram_llm": ell}
    exec(compile(src, str(MG), "exec"), ns)
    return ns


def test_experts_pin_wins_over_preset():
    ns = _load({"distill": {"model": "my-moe:q4"}, "triage": {"model": "my-moe:q4"}}, {})
    assert ns["LLM_MODEL"] == "my-moe:q4", f"experts pin ignored: {ns['LLM_MODEL']}"
    assert ns["SMALL_MODEL"] == "my-moe:q4", f"experts pin ignored: {ns['SMALL_MODEL']}"
    print("ok — experts pin resolves (no hardcoded llama3.1:8b)")


def test_env_override_wins():
    ns = _load({"distill": {"model": "my-moe:q4"}}, {"MG_LLM_MODEL": "engram-graph"})
    assert ns["LLM_MODEL"] == "engram-graph", "MG_LLM_MODEL must win — it's the num_ctx-variant hook"
    print("ok — MG_LLM_MODEL overrides the resolved default")


def test_reasoning_off_by_default():
    ns = _load({"distill": {"model": "m"}}, {})
    assert ns["REASONING_EFFORT"] == "", "reasoning injection must be opt-in"
    ns = _load({"distill": {"model": "m"}}, {"MG_REASONING_EFFORT": "high"})
    assert ns["REASONING_EFFORT"] == "high"
    print("ok — reasoning_effort opt-in via MG_REASONING_EFFORT")


if __name__ == "__main__":
    test_experts_pin_wins_over_preset(); test_env_override_wins(); test_reasoning_off_by_default()
