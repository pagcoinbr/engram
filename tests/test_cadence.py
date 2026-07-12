#!/usr/bin/env python3
"""Cadence changes (2026-07-12): harvest idle-grace (only finished chats) + the
stage_apply embedding cache (don't re-embed the whole store each frequent run)."""
import sys, types, importlib.util, json, tempfile, time, os
from pathlib import Path


def test_idle_grace():
    mai = types.ModuleType("memory_ai")
    mai.load = lambda: {"harvest": {"idle_minutes": 30}}
    mai.local_enabled = lambda c: True
    mai.expert_model = lambda r, c: "m"
    mai.ollama_generate = lambda p, role=None: "[]"
    mai.is_transient_fact = lambda n, d: False
    sys.modules["memory_ai"] = mai
    d = tempfile.mkdtemp(); os.environ["HOME"] = d; os.environ["CLAUDE_MEMORY_SLUG"] = "-t"
    proj = Path(d) / ".claude/projects/-t"; (proj / "memory/.staging").mkdir(parents=True)
    def tx(name):
        p = proj / f"{name}.jsonl"
        p.write_text(json.dumps({"type": "user", "message": {"role": "user",
                     "content": [{"type": "text", "text": "deploy port 9000 /home/x/y.py always"}]}}) + "\n")
        return p
    tx("active"); fin = tx("finished")
    old = time.time() - 3600; os.utime(fin, (old, old))
    sp = importlib.util.spec_from_file_location("mh", Path(__file__).resolve().parent.parent / "bin" / "memory_harvest.py")
    mh = importlib.util.module_from_spec(sp); sp.loader.exec_module(mh)
    mh.PROJ_DIR = proj; mh.MEM_DIR = proj / "memory"; mh.STAGING = proj / "memory/.staging"
    mh.STATE_PATH = proj / "hs.json"
    processed = {"n": 0}
    orig = mh.harvest_transcript
    mh.harvest_transcript = lambda *a, **k: (processed.__setitem__("n", processed["n"] + 1), orig(*a, **k))[1]
    sys.argv = ["mh"]
    mh.main()
    import shutil; shutil.rmtree(d)
    assert processed["n"] == 1, f"idle-grace: only the FINISHED chat should harvest, got {processed['n']}"
    print("ok — idle-grace (active chat skipped, finished harvested)")


def test_embed_cache():
    sys.modules["engram_secrets"] = types.ModuleType("engram_secrets")
    sys.modules["engram_secrets"].redact = lambda t: (t, 0)
    mai = types.ModuleType("memory_ai"); sys.modules["memory_ai"] = mai
    d = tempfile.mkdtemp(); os.environ["HOME"] = d; os.environ["CLAUDE_MEMORY_SLUG"] = "-t"
    mem = Path(d) / ".claude/projects/-t/memory"; mem.mkdir(parents=True)
    (mem / "a.md").write_text("---\nname: a\ndescription: x\n---\nbody\n")
    (mem / "b.md").write_text("---\nname: b\ndescription: y\n---\nbody\n")
    sp = importlib.util.spec_from_file_location("sa", Path(__file__).resolve().parent.parent / "bin" / "memory_stage_apply.py")
    sa = importlib.util.module_from_spec(sp)
    n = {"c": 0}
    mai.ollama_embed = lambda k, cfg=None: (n.__setitem__("c", n["c"] + 1) or [0.1, 0.2])
    sp.loader.exec_module(sa); sa.MEM_DIR = mem; sa._EMBED_CACHE = mem / ".cache.json"; sa.memory_ai = mai
    sa.existing_embeddings({}); first = n["c"]
    sa.existing_embeddings({}); second = n["c"] - first
    import shutil; shutil.rmtree(d)
    assert first == 2 and second == 0, f"cache: run1={first} (2), run2 re-embeds={second} (0)"
    print("ok — stage_apply embedding cache (no re-embed on unchanged store)")


if __name__ == "__main__":
    test_idle_grace(); test_embed_cache()
