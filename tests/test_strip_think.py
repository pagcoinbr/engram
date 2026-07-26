#!/usr/bin/env python3
"""Reasoning-model CoT must never reach a JSON-expecting caller (harvest/distill).

Two gaps this covers:
  1. _strip_think only matched a well-formed <think>...</think> PAIR. Ollama's chat
     template emits the opening tag itself, so `response` arrives mid-CoT and ends
     with a bare </think> — matching neither the regex nor the unclosed-opener
     branch, so entire reasoning blocks passed through untouched.
  2. _ollama_generate returned the raw `response`; only the llama.cpp path stripped.
"""
import sys, json, types, importlib.util
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("engram_llm", ROOT / "bin" / "engram_llm.py")
e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e)


def test_strip_well_formed_pair():
    assert e._strip_think('<think>reasoning</think>{"ok":true}') == '{"ok":true}'
    assert e._strip_think("<think>a</think>\n\nANSWER") == "ANSWER"


def test_strip_dangling_closer():
    """The real ollama shape — closer with no opener. This is the regression."""
    assert e._strip_think('weighing options...</think>\n\n{"ok":true}') == '{"ok":true}'
    assert e._strip_think("step 1\nstep 2\n</think>\n\nENGRAM_OK") == "ENGRAM_OK"


def test_cut_at_last_closer():
    assert e._strip_think("<think>a</think>mid</think>FINAL") == "FINAL"


def test_unclosed_opener_is_truncated_cot():
    """num_predict exhausted mid-CoT: drop from the opener, keep what preceded it."""
    assert e._strip_think("ANSWER<think>rambling that never ends") == "ANSWER"


def test_case_insensitive_and_passthrough():
    assert e._strip_think("<THINK>x</THINK>Y") == "Y"
    assert e._strip_think("  plain answer  ") == "plain answer"
    assert e._strip_think('{"ok":true}') == '{"ok":true}'
    assert e._strip_think("") == ""
    assert e._strip_think(None) == ""


def _fake_urlopen(payload):
    class R:
        def read(self): return json.dumps(payload).encode()
        def __enter__(self): return self
        def __exit__(self, *a): return False
    return lambda req, timeout=None: R()


def test_ollama_generate_strips_cot():
    """End of the actual path: _ollama_generate must not return CoT."""
    e.urllib.request.urlopen = _fake_urlopen(
        {"response": 'thinking out loud</think>\n\n{"ok":true}'})
    cfg = {"backend": "ollama", "tier": "small", "ollama": {"host": "http://x:11434"}}
    assert e._ollama_generate("p", "triage", cfg) == '{"ok":true}'


def test_think_flag_is_config_driven():
    """think defaults False (unchanged), and honours ollama.think when set."""
    sent = {}

    class R:
        def read(self): return json.dumps({"response": "ok"}).encode()
        def __enter__(self): return self
        def __exit__(self, *a): return False

    def capture(req, timeout=None):
        sent.update(json.loads(req.data.decode()))
        return R()
    e.urllib.request.urlopen = capture

    base = {"backend": "ollama", "tier": "small"}
    e._ollama_generate("p", "triage", {**base, "ollama": {}})
    assert sent["think"] is False, "default must stay False"
    e._ollama_generate("p", "triage", {**base, "ollama": {"think": True}})
    assert sent["think"] is True, "ollama.think: true must reach the request"


if __name__ == "__main__":
    for t in (test_strip_well_formed_pair, test_strip_dangling_closer,
              test_cut_at_last_closer, test_unclosed_opener_is_truncated_cot,
              test_case_insensitive_and_passthrough, test_ollama_generate_strips_cot,
              test_think_flag_is_config_driven):
        t()
    print("ok — CoT stripped on the ollama path; think flag config-driven")
