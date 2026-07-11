#!/usr/bin/env python3
"""C8 regression: final_fact_coverage (regex token classes) can read ~1.0 while a
whole PROSE fact was dropped. sentence_coverage must expose that, and
preserve_sources must make the merge lossless. Guards against coverage-gated
destructive merges."""
import sys, types, importlib.util, shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
mai = types.ModuleType("memory_ai"); mai.load = lambda: {}
sys.modules["memory_ai"] = mai
spec = importlib.util.spec_from_file_location("mdv", ROOT / "bin" / "memory_distill_verified.py")
mdv = importlib.util.module_from_spec(spec); spec.loader.exec_module(mdv)


def main():
    tmp = ROOT / "tests" / "_tmp_c8"; tmp.mkdir(exist_ok=True); mdv.MEM = tmp
    (tmp / "project_a.md").write_text(
        "---\nname: a\n---\nThe rebalancer must never fill dust below one thousand "
        "reais because fees eat the spread.\n")
    (tmp / "project_b.md").write_text("---\nname: b\n---\nDeploy on port 9000.\n")
    # LLM drops the prose fact, keeps only the tokened one
    mdv.ollama_stream = lambda p, c: ("Umbrella: deploy on port 9000.", "stop", 0.0)

    _, rep = mdv.distill_cluster("project", ["project_a.md", "project_b.md"])
    assert rep["final_fact_coverage"] >= 0.99, "token coverage should look perfect"
    assert rep["sentence_coverage"] < 1.0, "sentence_coverage must expose dropped prose"

    final2, rep2 = mdv.distill_cluster("project", ["project_a.md", "project_b.md"],
                                       preserve_sources=True)
    assert "never fill dust below one thousand reais" in final2, "preserve mode must be lossless"
    assert rep2["sources_preserved"] is True
    shutil.rmtree(tmp)
    print("ok — C8 distill preservation (token cov hid loss; sentence cov caught it; preserve lossless)")


if __name__ == "__main__":
    main()
