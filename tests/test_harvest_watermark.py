#!/usr/bin/env python3
"""Data-loss regression (Fable 1a/1b/1c): the harvest watermark advances only past
segments actually rendered (dense sessions keep their un-rendered remainder), holds
on an empty/garbage LLM response, and never lands mid-line."""
import sys, types, importlib.util, json, tempfile
from pathlib import Path

sys.modules.setdefault("memory_ai", types.ModuleType("memory_ai"))
spec = importlib.util.spec_from_file_location("mh", Path(__file__).resolve().parent.parent / "bin" / "memory_harvest.py")
mh = importlib.util.module_from_spec(spec); spec.loader.exec_module(mh)


def _ul(txt):
    return json.dumps({"type": "user", "message": {"role": "user",
                       "content": [{"type": "text", "text": txt}]}}) + "\n"


def main():
    mh.memory_ai.ollama_generate = lambda p, role=None: "[]"   # legit empty
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "t.jsonl"
        p.write_bytes((_ul("A" * 200) + _ul("B" * 200) + _ul("C" * 200)).encode())
        sz = p.stat().st_size
        st = {"files": {}}
        r1 = mh.harvest_transcript(p, "harvest", 250, st, True, False)
        wm1 = st["files"]["t.jsonl"]["offset"]
        assert r1["rendered"] < r1["segments"], "budget should force a partial render"
        assert wm1 < sz, "1a: watermark must NOT jump past un-rendered segments"
        r2 = mh.harvest_transcript(p, "harvest", 250, st, True, False)
        wm2 = st["files"]["t.jsonl"]["offset"]
        assert wm2 > wm1, "watermark must progress on the next run (no stall, no loss)"

        # 1b: garbage/empty response with content present must NOT advance
        st2 = {"files": {}}
        mh.memory_ai.ollama_generate = lambda p, role=None: "sorry, I can't help"
        mh.harvest_transcript(p, "harvest", 9999, st2, True, False)
        assert "t.jsonl" not in st2["files"], "1b: garbage response must hold the watermark"

        # 1c: a mid-append final line (no trailing newline) is not consumed
        p.write_bytes((_ul("A" * 50) + '{"partial":').encode())
        st3 = {"files": {}}
        mh.memory_ai.ollama_generate = lambda p, role=None: "[]"
        mh.harvest_transcript(p, "harvest", 9999, st3, True, False)
        assert st3["files"]["t.jsonl"]["offset"] < p.stat().st_size, "1c: must stop before the partial line"
    print("ok — harvest watermark (lossless render, garbage-hold, partial-line safe)")


if __name__ == "__main__":
    main()
