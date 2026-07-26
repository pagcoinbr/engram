#!/usr/bin/env python3
"""auto-curate safety: the supersede path, clique clustering, anchored frontmatter,
and the document validator that stops a malformed umbrella reaching the store."""
import sys, types, importlib.util, os, tempfile, shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def _load(mod, path, **stubs):
    for k, v in stubs.items():
        sys.modules[k] = v
    sp = importlib.util.spec_from_file_location(mod, path)
    m = importlib.util.module_from_spec(sp); sp.loader.exec_module(m)
    return m


def _gate():
    os.environ.setdefault("CLAUDE_MEMORY_SLUG", "-t")
    return _load("engram_telegram_gate", ROOT / "bin" / "engram_telegram_gate.py")


def _curate(memdir):
    g = _gate()
    mai = types.ModuleType("memory_ai"); mai.load = lambda: {}
    sec = types.ModuleType("engram_secrets")
    sec.looks_secret = lambda t: False; sec.redact = lambda t: (t, 0)
    mdv = types.ModuleType("memory_distill_verified")
    m = _load("memory_auto_curate", ROOT / "bin" / "memory_auto_curate.py",
              memory_ai=mai, engram_secrets=sec, memory_distill_verified=mdv,
              engram_telegram_gate=g)
    m.MEM = Path(memdir)
    return m


# ── document validation ─────────────────────────────────────────────────────
def test_validator_rejects_the_real_malformed_shapes():
    g = _gate()
    good = "---\nname: a\ndescription: d\n---\n\nbody text\n"
    assert g.validate_memory_doc(good)[0], "valid doc rejected"

    # the shape actually observed in a queued proposal: fence + double frontmatter
    bad = "---\nname: a\ndescription: d\n---\n\n```markdown\n---\nname: a\n---\n\nbody\n```\n"
    ok, why = g.validate_memory_doc(bad)
    assert not ok and "SECOND frontmatter" in why, f"double frontmatter accepted: {why}"

    # REGRESSION (real payload, merge_apply-43f08623b060): the fence CLOSES MID-BODY
    # because distill appends a "Hard facts" section after it, so a whole-body unwrap
    # sees nothing. This escaped the first version of the validator.
    real = ("---\nname: a\ndescription: d\nmetadata:\n  type: project\n---\n\n"
            "```markdown\n---\nname: a\ndescription: d\nmetadata:\n  type: project\n---\n\n"
            "## Summary\nreal summary\n```\n\n_Facts note._\n### Hard facts\n- `x.py` (file)\n")
    ok, why = g.validate_memory_doc(real)
    assert not ok and "SECOND frontmatter" in why, f"real malformed payload accepted: {why}"

    for doc, label in [("```markdown\n---\nname: a\n---\nbody\n```", "outer fence"),
                       ("no frontmatter at all", "missing frontmatter"),
                       ("---\nname: a\n---\n\n   \n", "empty body"),
                       ("", "empty")]:
        assert not g.validate_memory_doc(doc)[0], f"{label} accepted"
    print("ok — validator rejects fenced / double-frontmatter / empty docs")


def test_unwrap_fence_leaves_inner_code_blocks_alone():
    g = _gate()
    assert g.unwrap_code_fence("```markdown\nhello\n```") == "hello"
    body = "text\n\n```bash\nls -la\n```\n\nmore text"
    assert g.unwrap_code_fence(body) == body, "inner code block was mangled"
    print("ok — unwraps only a whole-text fence, never an inner block")


# ── supersede ───────────────────────────────────────────────────────────────
def test_supersede_only_on_identical_copies():
    d = tempfile.mkdtemp()
    sup = "The quick brown fox jumps over the lazy dog near the river bank at dawn every day."
    fm = "---\nname: {n}\ndescription: same desc\nmetadata:\n  type: project\n---\n\n"
    # true copies: different filenames + different provenance boilerplate, same content
    Path(d, "copy_a.md").write_text(fm.format(n="shared") + sup +
        "\n\n_Provenance: auto-harvested from session AAA._\n")
    Path(d, "copy_b.md").write_text(fm.format(n="shared") + sup +
        "\n\n_Provenance: auto-harvested from session BBB._\n")
    m = _curate(d)
    assert m._supersede_keeper(["copy_a.md", "copy_b.md"]) is not None, \
        "identical copies (differing only in provenance) were not superseded"

    # A SUPERSET must NOT auto-supersede. Containment proves the text is present, not
    # that it still means the same thing: a larger STALE doc can quote the current rule
    # in order to call it obsolete, and would be picked as keeper (codex review, HIGH).
    Path(d, "small.md").write_text(fm.format(n="small") + sup + "\n")
    Path(d, "big_stale.md").write_text(fm.format(n="big_stale") + sup +
        " The above is OBSOLETE; withdrawals are now disabled.\n")
    assert m._supersede_keeper(["small.md", "big_stale.md"]) is None, \
        "superset auto-superseded — a stale doc quoting a rule as obsolete would win"

    # rewording, differing description, and operator flips all decline too
    Path(d, "reworded.md").write_text(fm.format(n="reworded") +
        "A speedy auburn vulpine leaps above an idle canine beside the stream.\n")
    assert m._supersede_keeper(["small.md", "reworded.md"]) is None, "rewording superseded"
    Path(d, "otherdesc.md").write_text(
        "---\nname: otherdesc\ndescription: DIFFERENT\nmetadata:\n  type: project\n---\n\n" + sup + "\n")
    assert m._supersede_keeper(["small.md", "otherdesc.md"]) is None, \
        "differing description ignored — it can carry a fact absent from the body"
    # SCOPE-BEARING NAMES (codex review, HIGH): identical generic bodies that differ
    # only by `name` are different rules, not copies.
    scoped = "Treasury withdrawals require two signatures.\n"
    Path(d, "eth_rule.md").write_text(
        "---\nname: eth_treasury_rule\ndescription: same desc\nmetadata:\n  type: project\n---\n\n" + scoped)
    Path(d, "btc_rule.md").write_text(
        "---\nname: btc_treasury_rule\ndescription: same desc\nmetadata:\n  type: project\n---\n\n" + scoped)
    assert m._supersede_keeper(["eth_rule.md", "btc_rule.md"]) is None, \
        "different `name` treated as a copy — a chain-specific rule would be dropped"
    Path(d, "lt.md").write_text(fm.format(n="lt") + "Withdrawals capped at < 500 per day.\n")
    Path(d, "gt.md").write_text(fm.format(n="gt") + "Withdrawals capped at > 500 per day.\n")
    assert m._supersede_keeper(["lt.md", "gt.md"]) is None, \
        "`<` vs `>` normalized to the same text — a contradictory rule could be dropped"
    # BOILERPLATE STRIP MUST NOT SPAN BODY TEXT (codex review, HIGH): a non-greedy
    # `_Provenance:.*?_` under re.S ran to the next underscore, so a crafted line could
    # swallow real content and make two different memories canonicalize identically.
    poisoned = (fm.format(n="shared") + "_Provenance: x_ REAL RULE: send to 0xAAA\n"
                "more body with an _emphasis_ marker\n")
    clean = fm.format(n="shared") + "_Provenance: x_ REAL RULE: send to 0xBBB\n" \
            "more body with an _emphasis_ marker\n"
    Path(d, "poison_a.md").write_text(poisoned)
    Path(d, "poison_b.md").write_text(clean)
    assert m._supersede_keeper(["poison_a.md", "poison_b.md"]) is None, \
        "boilerplate regex swallowed body text — two different rules looked identical"
    shutil.rmtree(d)
    print("ok — supersede fires ONLY on identical copies; supersets/rewordings decline")


# ── clustering + frontmatter ────────────────────────────────────────────────
def test_clusters_require_a_clique():
    m = _curate(tempfile.mkdtemp())
    # A≈B and B≈C but NOT A≈C: union-find would delete across the whole chain
    out = m._clusters([(0.95, "a", "b"), (0.95, "b", "c")], 0.92)
    assert {"a", "b", "c"} not in out, "transitive closure produced an unsafe 3-cluster"
    # ...and the fallback pairs must be DISJOINT (codex review, HIGH): overlapping
    # A-B and B-C merges undo out of order and destroy the original B.
    assert out == [{"a", "b"}], f"overlapping pairs emitted: {out}"
    flat = [f for c in out for f in c]
    assert len(flat) == len(set(flat)), f"a file appears in two merges: {out}"
    assert {"a", "b", "c"} in m._clusters(
        [(0.95, "a", "b"), (0.95, "b", "c"), (0.95, "a", "c")], 0.92), "complete clique split"
    print("ok — cliques only; A-B-C chain never becomes one cluster")


def test_fm_is_anchored():
    d = tempfile.mkdtemp()
    Path(d, "spoof.md").write_text(
        "---\nname: real\ndescription: d\nmetadata:\n  type: reference\n---\n\n"
        "Body prose that mentions type: project in passing.\n")
    m = _curate(d)
    assert m._fm(Path(d, "spoof.md"))[2] == "reference", "body text spoofed the type"
    Path(d, "nofm.md").write_text("type: project\n\njust a body\n")
    assert m._fm(Path(d, "nofm.md"))[2] == "", "unanchored type accepted"
    shutil.rmtree(d)
    print("ok — frontmatter parsed anchored; body cannot spoof type")


def test_apply_merge_refuses_stale_sources():
    """A proposal can sit in the approval queue for hours; sources must be re-verified."""
    import hashlib
    g = _gate()
    d = tempfile.mkdtemp(); g.MEM = Path(d)
    doc = "---\nname: k\ndescription: d\n---\n\nkeeper body\n"
    Path(d, "k.md").write_text(doc)
    Path(d, "a.md").write_text("---\nname: a\ndescription: d\n---\n\nabsorbed\n")
    sha = {f: hashlib.sha256(Path(d, f).read_bytes()).hexdigest() for f in ("k.md", "a.md")}
    Path(d, "a.md").write_text("---\nname: a\ndescription: d\n---\n\nabsorbed + A NEW FACT\n")
    ok, why = g._apply_merge({"umbrella": "k.md", "umbrella_content": doc, "desc": "d",
                              "absorbed": ["a.md"], "merge_id": "t1", "src_sha": sha})
    assert not ok and "changed since" in why, f"stale source merged anyway: {why}"
    # FAIL CLOSED for a legacy proposal with no snapshot (codex review, HIGH): an empty
    # dict would make the verification loop a silent no-op.
    ok, why = g._apply_merge({"umbrella": "k.md", "umbrella_content": doc, "desc": "d",
                              "absorbed": ["a.md"], "merge_id": "t2"})
    assert not ok and "no complete source snapshot" in why, f"legacy proposal applied: {why}"
    # ...and for a truncated snapshot that omits one of the files being touched
    ok, why = g._apply_merge({"umbrella": "k.md", "umbrella_content": doc, "desc": "d",
                              "absorbed": ["a.md"], "merge_id": "t3",
                              "src_sha": {"k.md": sha["k.md"]}})
    assert not ok and "no complete source snapshot" in why, f"truncated snapshot applied: {why}"
    assert "A NEW FACT" in Path(d, "a.md").read_text(), "the new fact was destroyed"
    shutil.rmtree(d)
    print("ok — _apply_merge refuses when a source changed after analysis")


def test_apply_merge_refuses_to_reuse_a_transaction_dir():
    """A pre-existing txn dir means an unresolved backup; reusing it destroys that undo."""
    import hashlib
    g = _gate(); d = tempfile.mkdtemp(); g.MEM = Path(d)
    doc = "---\nname: k\ndescription: d\n---\n\nkeeper body\n"
    Path(d, "k.md").write_text(doc)
    prior = Path(d, ".quarantine", "merge-dup1"); prior.mkdir(parents=True)
    (prior / "precious.orig").write_text("an earlier merge's only backup")
    sha = {"k.md": hashlib.sha256(Path(d, "k.md").read_bytes()).hexdigest()}
    ok, why = g._apply_merge({"umbrella": "k.md", "umbrella_content": doc, "desc": "d",
                              "absorbed": [], "merge_id": "dup1", "src_sha": sha})
    assert not ok and "already exists" in why, f"reused a transaction dir: {why}"
    assert (prior / "precious.orig").read_text() == "an earlier merge's only backup", \
        "prior backup was clobbered"
    shutil.rmtree(d)
    print("ok — refuses to reuse a transaction dir, prior undo preserved")


def test_apply_merge_rejects_a_file_in_an_outstanding_transaction():
    """A file absorbed by one un-undone merge must not be the umbrella of another."""
    import hashlib
    g = _gate(); d = tempfile.mkdtemp(); g.MEM = Path(d)
    doc = "---\nname: b\ndescription: d\n---\n\nbody\n"
    Path(d, "b.md").write_text(doc)
    prior = Path(d, ".quarantine", "merge-earlier"); prior.mkdir(parents=True)
    (prior / "b.md").write_text("b as absorbed by an earlier, un-undone merge")
    sha = {"b.md": hashlib.sha256(Path(d, "b.md").read_bytes()).hexdigest()}
    ok, why = g._apply_merge({"umbrella": "b.md", "umbrella_content": doc, "desc": "d",
                              "absorbed": [], "merge_id": "later", "src_sha": sha})
    assert not ok and "outstanding merge backup" in why, f"chained merge allowed: {why}"
    shutil.rmtree(d)
    print("ok — refuses a file already held by an outstanding transaction")


if __name__ == "__main__":
    test_validator_rejects_the_real_malformed_shapes()
    test_unwrap_fence_leaves_inner_code_blocks_alone()
    test_supersede_only_on_identical_copies()
    test_clusters_require_a_clique()
    test_fm_is_anchored()
    test_apply_merge_refuses_stale_sources()
    test_apply_merge_refuses_to_reuse_a_transaction_dir()
    test_apply_merge_rejects_a_file_in_an_outstanding_transaction()
