#!/usr/bin/env python3
"""Phase 0 regression: the ccg backend routes generation through cc-gateway (sets
ANTHROPIC_BASE_URL + ANTHROPIC_API_KEY on the claude subprocess), falls back to raw
claude, and treats an auth failure as NON-retryable (no more 169k silent exit-1s)."""
import sys, types, importlib.util, subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# stub heavy optional deps engram_llm may import lazily; it only needs stdlib here
spec = importlib.util.spec_from_file_location("engram_llm", ROOT / "bin" / "engram_llm.py")
e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e)


def test_ccg_sets_proxy_env(monkey_env):
    captured = {}

    def fake_run(cmd, **kw):
        captured["env"] = kw.get("env") or {}
        return types.SimpleNamespace(stdout="PONG", returncode=0)
    e.subprocess.run = fake_run
    cfg = {"backend": "ccg", "fallback": "claude",
           "ccg": {"base_url": "http://ccg:8443", "api_key_env": "ENGRAM_CCG_KEY"},
           "claude": {"bin": "claude"}}
    monkey_env("ENGRAM_CCG_KEY", "ccg-client-key-123")
    out = e.generate("hi", "triage", cfg)
    assert out == "PONG"
    assert captured["env"]["ANTHROPIC_BASE_URL"] == "http://ccg:8443"
    assert captured["env"]["ANTHROPIC_API_KEY"] == "ccg-client-key-123"


def test_ccg_falls_back_to_raw_claude(monkey_env):
    calls = []

    def fake_run(cmd, **kw):
        env = kw.get("env") or {}
        calls.append("ccg" if env.get("ANTHROPIC_BASE_URL") else "raw")
        if env.get("ANTHROPIC_BASE_URL"):          # ccg attempt fails
            raise subprocess.CalledProcessError(1, cmd, stderr="upstream 500")
        return types.SimpleNamespace(stdout="RAW-OK", returncode=0)
    e.subprocess.run = fake_run
    cfg = {"backend": "ccg", "fallback": "claude",
           "ccg": {"base_url": "http://ccg:8443"}, "claude": {"bin": "claude", "retries": 1}}
    monkey_env("ENGRAM_CCG_KEY", "k"); monkey_env("ANTHROPIC_API_KEY", "k")
    out = e.generate("hi", "triage", cfg)
    assert out == "RAW-OK", out
    assert calls[0] == "ccg" and calls[-1] == "raw", calls


def test_ccg_auth_denial_fails_closed(monkey_env):
    """A ccg auth/policy denial must NOT fall back to raw claude (that would bypass
    the gateway's auth/audit/DLP boundary)."""
    calls = []

    def fake_run(cmd, **kw):
        env = kw.get("env") or {}
        calls.append("ccg" if env.get("ANTHROPIC_BASE_URL") else "raw")
        raise subprocess.CalledProcessError(1, cmd, stderr="Not logged in · Please run /login")
    e.subprocess.run = fake_run
    cfg = {"backend": "ccg", "fallback": "claude",
           "ccg": {"base_url": "http://ccg:8443"}, "claude": {"bin": "claude", "retries": 3}}
    monkey_env("ENGRAM_CCG_KEY", "k")
    try:
        e.generate("hi", "triage", cfg)
        assert False, "ccg auth denial should fail closed, not fall back"
    except e.BackendAuthError:
        pass
    assert calls == ["ccg"], f"must not have called raw claude: {calls}"


def test_ccg_requires_explicit_key_no_anthropic_fallback(monkey_env):
    """ccg must NOT implicitly use ANTHROPIC_API_KEY as the client key."""
    monkey_env("ENGRAM_CCG_KEY", "")            # not set
    monkey_env("ANTHROPIC_API_KEY", "real-anthropic-key")
    import os
    os.environ.pop("ENGRAM_CCG_KEY", None)
    cfg = {"ccg": {"base_url": "http://ccg:8443", "api_key_env": "ENGRAM_CCG_KEY"},
           "claude": {"bin": "claude"}}
    try:
        e._ccg_generate("hi", "triage", cfg)
        assert False, "should refuse without the configured ccg key"
    except RuntimeError as ex:
        assert "not set" in str(ex).lower() and "not authenticated" not in str(ex).lower()


def test_auth_failure_is_non_retryable(monkey_env):
    n = {"c": 0}

    def fake_run(cmd, **kw):
        n["c"] += 1
        raise subprocess.CalledProcessError(1, cmd, stderr="Not logged in · Please run /login")
    e.subprocess.run = fake_run
    cfg = {"claude": {"bin": "claude", "retries": 5}}
    try:
        e._claude_generate("hi", "triage", cfg)
        assert False, "should have raised"
    except e.BackendAuthError as ex:
        assert "auth" in str(ex).lower()
    assert n["c"] == 1, f"auth failure must NOT retry, ran {n['c']}x"


if __name__ == "__main__":
    import os
    saved = {}
    def monkey_env(k, v):
        saved.setdefault(k, os.environ.get(k)); os.environ[k] = v
    for t in (test_ccg_sets_proxy_env, test_ccg_falls_back_to_raw_claude,
              test_ccg_auth_denial_fails_closed, test_ccg_requires_explicit_key_no_anthropic_fallback,
              test_auth_failure_is_non_retryable):
        t(monkey_env)
    for k, v in saved.items():
        if v is None: os.environ.pop(k, None)
        else: os.environ[k] = v
    print("ok — ccg backend routing + auth-fail non-retryable")
