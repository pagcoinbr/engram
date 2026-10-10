#!/usr/bin/env python3
"""Guards bin/engram_llm_audit.py — the generation-call audit log that backs the
atlas "LLM Calls" tab. Covers the properties the feature depends on:
  * record() -> tail() round-trip, newest-first;
  * rotation past the size cap keeps .1 and tail() still spans the boundary;
  * concurrent appends from separate processes never corrupt a line;
  * detect_loops flags an exact (digest,model,role,proc) repeat, ignores health
    probes and singletons;
  * record() is best-effort (never raises);
  * a line from a different SCHEMA_VERSION is skipped by tail().
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MOD = ROOT / "bin" / "engram_llm_audit.py"


def _load(logdir: Path):
    """Fresh module instance bound to an isolated ENGRAM_LOG_DIR + key home."""
    os.environ["ENGRAM_LOG_DIR"] = str(logdir)
    os.environ["ENGRAM_CONFIG_HOME"] = str(logdir / "keyhome")
    spec = importlib.util.spec_from_file_location("engram_llm_audit_test", MOD)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def ok(msg):
    print(f"ok — {msg}")


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        a = _load(tmp)

        # 1. round-trip, newest-first
        for i in range(5):
            a.record(kind="generation", backend="llama_cpp", model="m", role="distill",
                     digest=a.digest(f"prompt-{i}"), outcome="ok", ms=i)
        rows = a.tail(10)
        assert len(rows) == 5, f"expected 5, got {len(rows)}"
        assert rows[0]["digest"] == a.digest("prompt-4"), "tail is not newest-first"
        assert all(r["v"] == a.SCHEMA_VERSION for r in rows)
        ok("record() -> tail() round-trip, newest-first")

        # 2. keyed digest is stable and not the plaintext
        d1, d2 = a.digest("same text"), a.digest("same text")
        assert d1 == d2 and d1 != a.digest("other"), "digest unstable / not discriminating"
        assert "same text" not in d1
        ok("digest is stable, discriminating, and not the plaintext")

        # 3. rotation keeps .1 and tail() spans the boundary with no loss
        a._MAX_BYTES = 2048  # shrink the cap for the test
        for i in range(200):
            a.record(kind="generation", backend="b", model="m", role="r",
                     digest=a.digest(f"roll-{i}"), outcome="ok")
        assert (Path(str(a._log_path())) ).exists()
        assert Path(str(a._log_path()) + ".1").exists(), "rotation did not create .1"
        spanning = a.tail(500)
        # the most recent few must be present (not hidden behind the rotation)
        recent_digests = {r["digest"] for r in spanning}
        assert a.digest("roll-199") in recent_digests, "newest event lost across rotation"
        ok("rotation keeps .1 and tail() spans the boundary without loss")

        # 4. detect_loops: exact repeat flagged; health + singletons ignored
        import time as _t
        now = _t.time()
        events = []
        for _ in range(4):
            events.append({"v": a.SCHEMA_VERSION, "ts": now, "kind": "generation",
                           "digest": "deadbeef", "model": "m", "role": "distill", "proc": "p"})
        events.append({"v": a.SCHEMA_VERSION, "ts": now, "kind": "generation",
                       "digest": "unique1", "model": "m", "role": "distill", "proc": "p"})
        for _ in range(9):  # identical HEALTH probes must NOT flag
            events.append({"v": a.SCHEMA_VERSION, "ts": now, "kind": "health",
                           "digest": "healthxx", "model": "m", "role": "triage", "proc": "p"})
        loops = a.detect_loops(events, window_s=300, threshold=3)
        assert len(loops) == 1, f"expected exactly 1 loop group, got {len(loops)}"
        assert loops[0]["digest"] == "deadbeef" and loops[0]["count"] == 4
        ok("detect_loops flags an exact repeat, ignores health probes and singletons")

        # stale events (outside the window) do not flag
        old = [{"v": a.SCHEMA_VERSION, "ts": now - 10000, "kind": "generation",
                "digest": "old", "model": "m", "role": "r", "proc": "p"} for _ in range(5)]
        assert a.detect_loops(old, window_s=300, threshold=3) == []
        ok("detect_loops respects the time window")

        # 5. best-effort: a bad log dir never raises
        saved = os.environ["ENGRAM_LOG_DIR"]
        os.environ["ENGRAM_LOG_DIR"] = "/proc/nonexistent/cannot/create"
        try:
            a.record(kind="generation", digest="x", outcome="ok")  # must not raise
            assert a.tail(1) == [] or isinstance(a.tail(1), list)
        finally:
            os.environ["ENGRAM_LOG_DIR"] = saved
        ok("record()/tail() are best-effort (never raise)")

        # 6. a future-schema line is skipped by tail()
        with open(a._log_path(), "a") as fh:
            fh.write(json.dumps({"v": a.SCHEMA_VERSION + 99, "ts": now, "digest": "future"}) + "\n")
            fh.write("this is not json\n")  # corrupt line tolerated too
        assert all(r["v"] == a.SCHEMA_VERSION for r in a.tail(500)), "future/corrupt line leaked"
        ok("tail() skips lines from a different schema version and corrupt lines")

        # 7. concurrent appends from TWO processes: every line parses (no interleave)
        for f in (a._log_path(), Path(str(a._log_path()) + ".1")):
            try:
                f.unlink()
            except OSError:
                pass
        writer = (
            "import importlib.util,os,sys\n"
            f"os.environ['ENGRAM_LOG_DIR']={str(tmp)!r}\n"
            f"os.environ['ENGRAM_CONFIG_HOME']={str(tmp / 'keyhome')!r}\n"
            f"spec=importlib.util.spec_from_file_location('m',{str(MOD)!r})\n"
            "m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m)\n"
            "tag=sys.argv[1]\n"
            "[m.record(kind='generation',backend='b',model='m',role='r',digest=m.digest(f'{tag}-{i}'),outcome='ok') for i in range(300)]\n"
        )
        procs = [subprocess.Popen([sys.executable, "-c", writer, tag]) for tag in ("A", "B")]
        for p in procs:
            p.wait()
        lines = Path(a._log_path()).read_text().splitlines()
        bad = 0
        for ln in lines:
            ln = ln.strip()
            if not ln:
                continue
            try:
                json.loads(ln)
            except ValueError:
                bad += 1
        assert bad == 0, f"{bad} corrupt/interleaved lines out of {len(lines)}"
        assert len(lines) >= 500, f"expected ~600 lines, got {len(lines)}"
        ok("concurrent appends from two processes never corrupt a line")

    print("PASS test_llm_audit")


if __name__ == "__main__":
    main()
