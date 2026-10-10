#!/usr/bin/env python3
"""The Rust and Python tenant models must derive identical names.

Python WRITES the indexes (the daemon, graph_sync, vector_sync) and Rust READS
them. If the two disagree about which Qdrant collection or which Graphiti group
belongs to a tenant, the write goes one place and the read goes another — and the
symptom is not an error, it is an index that looks permanently empty. Exactly the
failure mode `tests/test_embed_space.py` exists to prevent for the embedding
fingerprint, so it gets the same treatment: literal expected values asserted from
both sides, which is the only form of this check that cannot drift.

The Rust counterpart is `each_tenant_addresses_its_own_collections_and_graph_group`
in crates/engram-tenant/src/lib.rs, which asserts these same strings.
"""
import importlib.util
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The literals both languages must produce for the two-tenant config below.
# Changing either side without the other is the bug this file catches.
EXPECTED = {
    "work-company-x": {
        "memory_collection": "engram_memory__work-company-x",
        "wiki_collection": "engram_wiki__work-company-x",
        "graph_group": "work-company-x",
    },
    "homelab": {
        "memory_collection": "engram_memory__homelab",
        "wiki_collection": "engram_wiki__homelab",
        "graph_group": "homelab",
    },
}

CONFIG = """\
local_enabled: true
vector_store:
  collection: engram_memory
  wiki_collection: engram_wiki
tenants:
  work-company-x:
    slugs: ['-root-MJSV']
    vault: {work}
  homelab:
    slugs: ['-root', '-root-ASCP']
    vault: {home}
"""

FAILURES = []


def check(label, got, want):
    if got != want:
        FAILURES.append(f"{label}: got {got!r}, want {want!r}")


def load(home, config):
    """Import bin/engram_tenant.py against a throwaway $HOME."""
    claude = Path(home) / ".claude"
    claude.mkdir(parents=True, exist_ok=True)
    for f in (ROOT / "bin").glob("*.py"):
        (claude / f.name).write_bytes(f.read_bytes())
    (claude / "engram.yaml").write_text(config)
    sys.path.insert(0, str(claude))
    spec = importlib.util.spec_from_file_location(
        "engram_tenant", ROOT / "bin" / "engram_tenant.py"
    )
    mod = importlib.util.module_from_spec(spec)
    sys.modules["engram_tenant"] = mod
    spec.loader.exec_module(mod)
    return mod


def main():
    with tempfile.TemporaryDirectory() as home:
        os.environ["HOME"] = home
        os.environ.pop("ENGRAM_TENANT", None)
        work = Path(home) / "vaults" / "work"
        homelab = Path(home) / "vaults" / "homelab"
        for vault in (work, homelab):
            (vault / "_agent").mkdir(parents=True, exist_ok=True)
        et = load(home, CONFIG.format(work=work, home=homelab))

        # 1. The names both languages derive, pinned literally.
        for name, want in EXPECTED.items():
            t = et.resolve(requested=name)
            check(f"{name} memory_collection", t.memory_collection, want["memory_collection"])
            check(f"{name} wiki_collection", t.wiki_collection, want["wiki_collection"])
            check(f"{name} graph_group", t.graph_group, want["graph_group"])

        # 2. No named tenant may address the shared pre-tenancy names, or the
        #    migration would read its own un-migrated data back.
        for name in EXPECTED:
            t = et.resolve(requested=name)
            for got in (t.memory_collection, t.wiki_collection, t.graph_group):
                if got in ("engram_memory", "engram_wiki", et.LEGACY_GRAPH_GROUP):
                    FAILURES.append(f"{name} still addresses the shared name {got!r}")

        # 3. An absent tenant is refused, even though... well, there are two
        #    here; the single-tenant case is covered on the Rust side.
        try:
            et.resolve()
            FAILURES.append("an absent tenant was not refused")
        except et.TenantError as error:
            if "required" not in str(error):
                FAILURES.append(f"wrong error for an absent tenant: {error}")
        try:
            et.resolve(requested="nope")
            FAILURES.append("an unknown tenant was not refused")
        except et.TenantError as error:
            if "unknown tenant" not in str(error):
                FAILURES.append(f"wrong error for an unknown tenant: {error}")

        # 4. Slug ownership, including the refusal that keeps one agent out of
        #    another's store.
        wk = et.resolve(requested="work-company-x")
        hl = et.resolve(requested="homelab")
        check("owns own slug", wk.owns_slug("-root-MJSV"), True)
        check("rejects foreign slug", wk.owns_slug("-root"), False)
        check("choose derived", wk.choose_slug(None, "-root-MJSV"), "-root-MJSV")
        # the lone store is used when the working directory is elsewhere
        check("choose sole store", wk.choose_slug(None, "-somewhere"), "-root-MJSV")
        try:
            wk.choose_slug("-root", "-root-MJSV")
            FAILURES.append("a foreign --slug was accepted")
        except et.TenantError:
            pass
        # homelab owns two, so an unmatched directory has to ask
        try:
            hl.choose_slug(None, "-somewhere-else")
            FAILURES.append("an ambiguous store was guessed rather than refused")
        except et.TenantError as error:
            if "--slug" not in str(error):
                FAILURES.append(f"unhelpful ambiguity error: {error}")

        # 5. The vault boundary on disk. A symlink out of the vault is the case
        #    no database filter would catch.
        (work / "Runbooks").mkdir(exist_ok=True)
        (work / "Runbooks" / "DNS.md").write_text("# DNS\n")
        (homelab / "Secrets").mkdir(exist_ok=True)
        (homelab / "Secrets" / "Keys.md").write_text("# Keys\n")
        wk.resolve_in_vault("Runbooks/DNS.md")  # must not raise
        os.symlink(homelab, work / "leak")
        for bad in ("leak", "leak/Secrets/Keys.md", "../homelab/Secrets/Keys.md", "/etc/passwd"):
            try:
                wk.resolve_in_vault(bad)
                FAILURES.append(f"vault escape allowed via {bad!r}")
            except et.TenantError:
                pass

        # 6. Agent writes are confined twice: inside the vault, and inside the
        #    agent subtree.
        wk.resolve_for_agent_write("_agent/findings/new.md")  # must not raise
        for bad in ("Runbooks/DNS.md", "leak/x.md"):
            try:
                wk.resolve_for_agent_write(bad)
                FAILURES.append(f"agent write allowed outside the subtree: {bad!r}")
            except et.TenantError:
                pass

        # 7. A pre-tenancy install keeps its historical names and takes any slug.
        legacy = load(home, "local_enabled: true\n")
        lt = legacy.resolve()
        check("legacy is legacy", lt.is_legacy, True)
        check("legacy collection", lt.memory_collection, "engram_memory")
        check("legacy group", lt.graph_group, "canonical")
        check("legacy takes any slug", lt.choose_slug(None, "-root-anything"), "-root-anything")

    if FAILURES:
        print("FAIL test_tenant_parity")
        for failure in FAILURES:
            print(f"  - {failure}")
        return 1
    print("PASS test_tenant_parity")
    return 0


if __name__ == "__main__":
    sys.exit(main())
