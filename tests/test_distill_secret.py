#!/usr/bin/env python3
"""S1 regression: a secret in a source note must never reach the distilled draft,
the appendix, or the LLM prompt — for local OR remote backends. We stub the LLM
to ECHO its prompt back (worst case: a leaky/misbehaving model), so if any secret
survived into members_text it would show up in the draft."""
import sys, types, importlib.util
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# stub memory_ai (config loader) so no real backend/config is needed
mai = types.ModuleType("memory_ai")
mai.load = lambda: {}
mai.ollama_host = lambda cfg: "http://x"
mai.expert_model = lambda role, cfg: "stub"
sys.modules["memory_ai"] = mai

spec = importlib.util.spec_from_file_location("mdv", ROOT / "bin" / "memory_distill_verified.py")
mdv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mdv)

SECRET = "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
NOTE = f"""---
name: project_thing
description: a thing
metadata:
  type: project
---
Deploy runs on port 9000 at /home/pagcoin/thing.py.
GITHUB_TOKEN={SECRET}
The webhook_secret = SuperSecretValueGoesHere123
"""


def main():
    tmp = ROOT / "tests" / "_tmp_mem"
    tmp.mkdir(exist_ok=True)
    mdv.MEM = tmp
    (tmp / "project_thing.md").write_text(NOTE)

    # stub the LLM to echo the prompt it was given (worst-case leak surface)
    mdv.ollama_stream = lambda prompt, cfg: (prompt, "stop", 0.0)

    final, report = mdv.distill_cluster("project", ["project_thing.md"])

    assert SECRET not in final, "SECRET leaked into distilled draft!"
    assert "SuperSecretValueGoesHere123" not in final, "assigned secret leaked!"
    assert "«redacted-secret»" in final, "expected redaction marker in draft"
    # durable non-secret facts must survive
    assert "9000" in final and "thing.py" in final, "durable facts lost"
    assert report["secrets_redacted"] >= 1, "report should count redactions"
    (tmp / "project_thing.md").unlink()
    tmp.rmdir()
    print(f"ok — S1 distill secret redaction (redacted={report['secrets_redacted']})")


if __name__ == "__main__":
    main()
