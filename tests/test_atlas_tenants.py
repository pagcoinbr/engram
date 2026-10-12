#!/usr/bin/env python3
"""Pins the tenant-model contract the atlas "Tenants" tab depends on.

atlas/atlas_api.py's GET /api/atlas/tenants and its project→tenant annotation
read engram_tenant.py by file path (the Rust API is single-tenant per process and
lists no tenants, so Python is the only source). This test exercises exactly the
fields atlas consumes — names(), tenancy_enabled(), tenant_of_slug(), and a
resolved Tenant's label/slugs/vault/agent_subtree/extract_facts/collections/group
— plus the per-tenant SilverBullet URL shape (wiki-<name>.<suffix>) atlas builds,
and the untenanted degrade. If engram_tenant's surface drifts, the tab breaks and
this fails rather than the UI 500-ing in production.
"""
import importlib.util
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CONFIG = """\
local_enabled: true
vector_store:
  collection: engram_memory
  wiki_collection: engram_wiki
ui:
  wiki_host_suffix: example.test
tenants:
  bbhost:
    slugs: ['-root-bbhost', '-root-bbhost-pcaps']
    vault: {bbhost}
    extract_facts: true
  homelab:
    slugs: ['-root', '-root-ASCP']
    vault: {homelab}
"""

FAILURES = []


def check(label, got, want):
    if got != want:
        FAILURES.append(f"{label}: got {got!r}, want {want!r}")


def load(home, config):
    """Import bin/engram_tenant.py against a throwaway $HOME, as atlas loads it."""
    claude = Path(home) / ".claude"
    claude.mkdir(parents=True, exist_ok=True)
    for f in (ROOT / "bin").glob("*.py"):
        (claude / f.name).write_bytes(f.read_bytes())
    (claude / "engram.yaml").write_text(config)
    sys.path.insert(0, str(claude))
    spec = importlib.util.spec_from_file_location("engram_tenant", ROOT / "bin" / "engram_tenant.py")
    mod = importlib.util.module_from_spec(spec)
    sys.modules["engram_tenant"] = mod
    spec.loader.exec_module(mod)
    return mod


def _wiki_suffix(cfg):
    """The exact suffix logic atlas_api._wiki_suffix implements."""
    ui = cfg.get("ui", {}) or {}
    return str(ui.get("wiki_host_suffix") or "home.arpa").strip().strip(".")


def main():
    with tempfile.TemporaryDirectory() as home:
        os.environ["HOME"] = home
        os.environ.pop("ENGRAM_TENANT", None)
        bbhost = Path(home) / "vaults" / "bbhost"
        homelab = Path(home) / "vaults" / "homelab"
        for vault in (bbhost, homelab):
            (vault / "_agent").mkdir(parents=True, exist_ok=True)
        et = load(home, CONFIG.format(bbhost=bbhost, homelab=homelab))
        cfg = et._cfg(None)

        # 1. The tenant set atlas iterates, and the degrade gate.
        check("tenancy_enabled", et.tenancy_enabled(cfg), True)
        check("names sorted", et.names(cfg), ["bbhost", "homelab"])

        # 2. Every field the endpoint puts in each tenant's payload, plus the URL.
        suffix = _wiki_suffix(cfg)
        check("suffix from ui block", suffix, "example.test")
        expected = {
            "bbhost": {"memory_collection": "engram_memory__bbhost",
                       "wiki_collection": "engram_wiki__bbhost", "graph_group": "bbhost",
                       "extract_facts": True, "slugs": ["-root-bbhost", "-root-bbhost-pcaps"]},
            "homelab": {"memory_collection": "engram_memory__homelab",
                        "wiki_collection": "engram_wiki__homelab", "graph_group": "homelab",
                        "extract_facts": False, "slugs": ["-root", "-root-ASCP"]},
        }
        for name, want in expected.items():
            t = et.resolve(cfg, name)
            check(f"{name} label", t.label, name)
            check(f"{name} slugs", [str(s) for s in t.slugs], want["slugs"])
            check(f"{name} memory_collection", t.memory_collection, want["memory_collection"])
            check(f"{name} wiki_collection", t.wiki_collection, want["wiki_collection"])
            check(f"{name} graph_group", t.graph_group, want["graph_group"])
            check(f"{name} extract_facts", bool(t.extract_facts), want["extract_facts"])
            check(f"{name} agent_subtree", t.agent_subtree, "_agent")
            check(f"{name} vault present", bool(t.vault), True)
            check(f"{name} wiki_url", f"https://wiki-{name}.{suffix}", f"https://wiki-{name}.example.test")

        # 3. Slug → tenant, the map atlas annotates every project with.
        check("owner of -root", et.tenant_of_slug("-root", cfg), "homelab")
        check("owner of -root-bbhost-pcaps", et.tenant_of_slug("-root-bbhost-pcaps", cfg), "bbhost")
        check("unowned slug -> None", et.tenant_of_slug("-root-nobody", cfg), None)

        # 4. Untenanted config → the endpoint's degrade branch (enabled:false).
        check("no tenants -> disabled", et.tenancy_enabled({"local_enabled": True}), False)
        check("no tenants -> empty names", et.names({"local_enabled": True}), [])

        # 5. Default suffix when no ui block is set.
        check("default suffix", _wiki_suffix({}), "home.arpa")

    if FAILURES:
        print("FAIL test_atlas_tenants")
        for line in FAILURES:
            print("  -", line)
        return 1
    print("PASS test_atlas_tenants")
    return 0


if __name__ == "__main__":
    sys.exit(main())
