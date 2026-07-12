#!/usr/bin/env python3
"""zero-cmd #2: explicit 'make this a skill' intent stamps promote:requested at harvest,
which bypasses ONLY the maturity gate in the promoter (keeps the procedure gate) —
replacing the on-demand /memory-to-skill command."""
import sys, importlib.util, tempfile, shutil
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent


def test_intent_regex():
    sp = importlib.util.spec_from_file_location("mh", ROOT / "bin" / "memory_harvest.py")
    # stub memory_ai so the module imports without config
    sys.modules.setdefault("memory_ai", type(sys)("memory_ai"))
    mh = importlib.util.module_from_spec(sp); sp.loader.exec_module(mh)
    for yes in ["make this a skill", "turn it into a skill", "save this as a runbook",
                "so you can run it next time", "remember these steps as a procedure"]:
        assert mh.PROMOTE_INTENT_RE.search(yes), f"should detect intent: {yes}"
    for no in ["the deploy runs on port 9000", "make a backup of the db", "skill issue"]:
        assert not mh.PROMOTE_INTENT_RE.search(no), f"false positive: {no}"
    print("ok — promote intent regex")


def test_promoter_bypass():
    sp = importlib.util.spec_from_file_location("pc", ROOT / "bin" / "memory_promote_candidates.py")
    pc = importlib.util.module_from_spec(sp); sp.loader.exec_module(pc)
    d = Path(tempfile.mkdtemp())
    base = dict(status="provisional", frequency=0, confidence=0.4, survival=0,
                age_days=1, review_interval_days=7)
    proc = "## Steps\n```sh\n./deploy.sh\n```\nrun ./deploy.sh then systemctl restart x\n"
    (d / "req.md").write_text("---\nname: req\npromote: requested\n---\n" + proc)
    (d / "notag.md").write_text("---\nname: notag\n---\n" + proc)
    (d / "nostep.md").write_text("---\nname: nostep\npromote: requested\n---\njust a fact, no steps\n")
    assert pc.evaluate({**base, "name": "req.md"}, d, 15, 1.0)["eligible"], "tag should bypass maturity"
    assert not pc.evaluate({**base, "name": "notag.md"}, d, 15, 1.0)["eligible"], "no tag -> gated by maturity"
    assert not pc.evaluate({**base, "name": "nostep.md"}, d, 15, 1.0)["eligible"], "tag but no procedure -> still gated"
    shutil.rmtree(d)
    print("ok — promoter maturity-bypass (procedure gate kept)")


if __name__ == "__main__":
    test_intent_regex(); test_promoter_bypass()
