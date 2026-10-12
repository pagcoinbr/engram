"""engram_tenant.py — agent identity, and the boundary between identities.

The Python mirror of the Rust `engram-tenant` crate. Both have to exist and both
have to agree, for the same reason the embedding-space fingerprint is pinned in
two languages: Python *writes* the indexes (the daemon, graph_sync, vector_sync)
and Rust *reads* them. If the two disagree about which collection or which
Graphiti group belongs to a tenant, the write goes one place and the read goes
another — and the symptom is not an error, it is an index that looks empty.

A tenant is one agent's world: the memory stores it owns, the Obsidian vault it
reads, the Qdrant collections it searches, the Neo4j group it writes.

Two rules carried over from the Rust side deliberately:

- **An absent tenant is never a default.** With tenancy configured, a missing
  tenant is refused even when only one is defined. "Obviously the only one" is
  how a default gets established that silently becomes wrong the day a second
  tenant appears.
- **Legacy is a tenant too.** An install with no ``tenants:`` block — which is
  every install that exists today — resolves to a legacy tenant reading the
  configured collection and Graphiti's historical ``canonical`` group, so the
  upgrade path is one code path rather than a branch at every call site.

See ``tests/test_tenant_parity.py``, which pins the names both languages derive.
"""
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))

# Graphiti's historical single group: engram wrote this literal for every memory
# in every store, which is why the graph leg crossed projects. Retained ONLY for
# the legacy tenant.
LEGACY_GRAPH_GROUP = "canonical"

DEFAULT_MEMORY_COLLECTION = "engram_memory"
DEFAULT_WIKI_COLLECTION = "engram_wiki"
DEFAULT_AGENT_SUBTREE = "_agent"


class TenantError(Exception):
    """A tenant could not be resolved, or a path left its vault."""


def _cfg(cfg=None):
    if cfg is not None:
        return cfg
    import memory_ai  # the config loader; imported lazily to avoid a cycle

    return memory_ai.load()


def tenancy_enabled(cfg=None) -> bool:
    """Whether this install uses tenancy at all.

    An empty/absent ``tenants:`` block means "tenancy off", never "no tenant
    matched" — that distinction is the whole upgrade path.
    """
    return bool((_cfg(cfg).get("tenants") or {}))


def _tenants(cfg) -> dict:
    block = cfg.get("tenants") or {}
    return block if isinstance(block, dict) else {}


class Tenant:
    """A resolved agent identity. Build with :func:`resolve`."""

    def __init__(self, name, cfg):
        store = _tenants(cfg)
        entry = (store.get(name) or {}) if name else {}
        vector = cfg.get("vector_store") or {}
        memory_base = (vector.get("collection") or DEFAULT_MEMORY_COLLECTION).strip()
        wiki_base = (vector.get("wiki_collection") or DEFAULT_WIKI_COLLECTION).strip()

        self.name = name  # None == the legacy, pre-tenancy identity
        self.slugs = [str(s).strip() for s in (entry.get("slugs") or []) if str(s).strip()]
        vault = (entry.get("vault") or "").strip()
        self.vault = Path(vault) if vault else None
        self.agent_subtree = (entry.get("agent_subtree") or DEFAULT_AGENT_SUBTREE).strip()
        self.extract_facts = bool(entry.get("extract_facts"))
        # Suffixed per tenant, matching Rust's `format!("{base}__{name}")`. The
        # collection name IS the tenant boundary for vectors: reaching another
        # identity's points requires naming its collection, rather than merely
        # forgetting a filter clause.
        self.memory_collection = f"{memory_base}__{name}" if name else memory_base
        self.wiki_collection = f"{wiki_base}__{name}" if name else wiki_base

    @property
    def is_legacy(self) -> bool:
        return self.name is None

    @property
    def label(self) -> str:
        return self.name or "(untenanted)"

    @property
    def graph_group(self) -> str:
        """The Graphiti ``group_id`` / native ``tenant`` property."""
        return self.name or LEGACY_GRAPH_GROUP

    def owns_slug(self, slug: str) -> bool:
        """The legacy tenant owns everything: on a pre-tenancy install there is
        no boundary, and every existing caller passes a slug resolved the usual
        way. A named tenant owns only what it declares."""
        return self.is_legacy or (slug or "").strip() in self.slugs

    def choose_slug(self, cli=None, derived="") -> str:
        """Which of this tenant's stores to use.

        Mirrors ``Tenant::choose_slug``. The derived slug comes from the working
        directory, which on a multi-tenant host is frequently some *other*
        tenant's project — so it is a hint, not an authority.
        """
        cli = (cli or "").strip()
        if cli:
            if not self.owns_slug(cli):
                raise TenantError(
                    f"tenant {self.label!r} does not own memory store {cli!r}; "
                    "it belongs to another identity"
                )
            return cli
        derived = (derived or "").strip()
        if self.is_legacy or self.owns_slug(derived):
            return derived
        if len(self.slugs) == 1:
            return self.slugs[0]
        raise TenantError(
            f"tenant {self.label!r} owns several memory stores "
            f"({', '.join(self.slugs)}) and none matches the current directory "
            f"({derived!r}); pass --slug to choose one"
        )

    def vault_root(self) -> Path:
        if self.vault is None:
            raise TenantError(f"tenant {self.label!r} has no vault configured")
        try:
            return self.vault.resolve(strict=True)
        except OSError as error:
            raise TenantError(
                f"vault for tenant {self.label!r} is not readable at {self.vault}: {error}"
            ) from error

    def resolve_in_vault(self, relative) -> Path:
        """Resolve a vault-relative path, refusing anything that leaves the vault.

        This is the function that makes the isolation claim true on disk. A
        symlink inside one tenant's vault pointing at another's, or a ``..``
        walked up out of it, would otherwise hand an agent the other identity's
        documents while every database filter stayed perfectly correct.
        """
        root = self.vault_root()
        relative = Path(relative)
        # Shape first: cheap, no filesystem, and it rejects the ordinary
        # ../../etc/passwd case before anything is opened.
        if relative.is_absolute() or any(part in ("..", "~") for part in relative.parts):
            raise TenantError(f"{relative} must be a relative path inside the vault")
        # Then resolve symlinks and re-check. Checking the joined path textually
        # would pass a `leak -> /vaults/other` symlink, since it IS under root.
        resolved = (root / relative).resolve()
        if root != resolved and root not in resolved.parents:
            raise TenantError(f"{relative} escapes the vault of tenant {self.label!r}")
        return resolved

    def resolve_for_agent_write(self, relative) -> Path:
        """As :meth:`resolve_in_vault`, and additionally confined to the
        agent-writable subtree.

        Two separate checks on purpose: containment keeps an agent inside its own
        vault, and this keeps it out of the human-authored part of that vault.
        """
        resolved = self.resolve_in_vault(relative)
        writable = self.resolve_in_vault(self.agent_subtree)
        if writable != resolved and writable not in resolved.parents:
            raise TenantError(
                f"{relative} is outside the agent-writable subtree "
                f"{self.agent_subtree!r}; agents may only write there"
            )
        return resolved


def resolve(cfg=None, requested=None) -> Tenant:
    """Resolve the active tenant, or explain precisely why there isn't one.

    ``requested`` is the ``--tenant`` flag; ``ENGRAM_TENANT`` is consulted when
    it is absent. Every refusal lists the configured tenants, because the
    operator's next action is always to pick one.
    """
    cfg = _cfg(cfg)
    requested = (requested or os.environ.get("ENGRAM_TENANT") or "").strip()
    tenants = _tenants(cfg)
    if not tenants:
        if requested:
            raise TenantError(
                f"--tenant {requested!r} was given but no tenants are configured; "
                "add a 'tenants:' block to engram.yaml first"
            )
        return Tenant(None, cfg)
    available = ", ".join(sorted(tenants))
    if not requested:
        raise TenantError(
            f"--tenant is required: this install defines tenants ({available}). "
            "Pass --tenant <name> or set ENGRAM_TENANT"
        )
    if requested not in tenants:
        raise TenantError(
            f"unknown tenant {requested!r}; configured tenants are {available}"
        )
    return Tenant(requested, cfg)


def names(cfg=None) -> list:
    """Configured tenant names, sorted — what the daemon iterates."""
    return sorted(_tenants(_cfg(cfg)))


def tenant_of_slug(slug: str, cfg=None):
    """Which tenant owns ``slug``, or None."""
    cfg = _cfg(cfg)
    for name, entry in _tenants(cfg).items():
        owned = [str(s).strip() for s in ((entry or {}).get("slugs") or [])]
        if (slug or "").strip() in owned:
            return name
    return None
