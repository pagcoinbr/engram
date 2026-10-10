"""Per-slug scoping of the graph ingest's on-disk state.

The extractor (``graph_sync.py``), the inserter (``memory_graph_insert.py``) and
the maintenance pass (``graph_maint.py``) each persist three things beside the
code: the extraction JSON cache, ``insert_state.json`` (the per-file "done" set +
the entity name→uuid map) and ``sync_state.json`` (file → sha256 last inserted).

These used to live FLAT in the graph directory, one copy shared by every store.
That was correct when a daemon owned a single store. With tenants configured the
daemon runs the ingest ONCE PER SLUG, each with its own per-slug ``MEM_DIR`` —
but the state stayed shared. So an insert for store B would read an extraction
produced for store A, look for A's ``.md`` under B's ``MEM_DIR``, hit
``skip (no .md)``, never stamp "done", and the extractor would re-harvest that
memory on the very next cycle. The result was the same ~100 prompts re-sent to
the local model around the clock, forever, while ``insert_state.json`` stayed
frozen at whatever the one store that happened to line up had reached.

Scoping the three artifacts by slug gives each store its own "done" set, so a
memory is extracted exactly once. ``migrate_legacy`` folds any pre-existing flat
state in, assigning each done file to the store it was actually inserted from
(disambiguated by the recorded sha), so the files already in the graph are not
re-inserted (which would duplicate episodes).

Stdlib only, on purpose: ``graph_sync.py`` runs under the daemon's interpreter
with no graphiti installed, while the inserter runs in the graphiti venv — both
import this.
"""
import hashlib
import json
import os
import sys
from pathlib import Path


def _projects() -> Path:
    # Resolved per call (not at import) so tests can point $HOME at a sandbox.
    return Path.home() / ".claude" / "projects"


def slug_from_argv_env() -> str:
    """The store this run owns: ``--slug`` wins, then ``$CLAUDE_MEMORY_SLUG``,
    then a $HOME-derived default (the pre-tenancy single-store case)."""
    a = sys.argv[1:]
    if "--slug" in a:
        i = a.index("--slug")
        if i + 1 < len(a) and not a[i + 1].startswith("--"):
            return a[i + 1]
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")


def state_dir(here: Path, slug: str) -> Path:
    return Path(here) / "state" / slug


def paths(here: Path, slug: str):
    """(extractions_dir, insert_state.json, sync_state.json) for one slug."""
    base = state_dir(here, slug)
    return base / "extractions", base / "insert_state.json", base / "sync_state.json"


def _store_slugs() -> list:
    projects = _projects()
    if not projects.exists():
        return []
    return sorted(p.name for p in projects.iterdir() if (p / "memory").is_dir())


def _owner_slug(fname: str, want_sha, slugs: list):
    """Which store a done/extracted file belongs to, or ``None`` to leave it
    unassigned (orphan). A single store holding the file is unambiguous. When
    SEVERAL stores hold a same-named file, assign it to the one whose copy hashes
    to the sha recorded at insert time; if a sha was recorded but matches NONE of
    them (the file was edited in every store since), orphan it rather than guess
    ``candidates[0]`` — a wrong guess files "done" under the wrong store and the
    real owner later re-inserts it as a duplicate episode. With no recorded sha to
    disambiguate, fall back to the first candidate (best effort)."""
    projects = _projects()
    candidates = [s for s in slugs if (projects / s / "memory" / fname).is_file()]
    if not candidates:
        return None
    if len(candidates) == 1:
        return candidates[0]            # only one store has it: unambiguous
    if want_sha:
        for s in candidates:
            try:
                if hashlib.sha256((projects / s / "memory" / fname).read_bytes()).hexdigest() == want_sha:
                    return s
            except OSError:
                pass
        return None                     # several candidates, sha matched none: don't guess
    return candidates[0]                # no sha to disambiguate: best effort


def migrate_legacy(here: Path) -> list:
    """One-time, idempotent: split flat ``insert_state.json`` / ``sync_state.json``
    / ``extractions/`` into per-slug dirs, then retire the flat files so this is a
    no-op forever after. Returns human-readable notes (empty when nothing to do).

    flock-guarded on a sidecar lock so two per-tenant ingest processes starting at
    once cannot both migrate. Best-effort: any failure leaves the flat files in
    place (so a later run retries) rather than raising into the caller."""
    import fcntl
    here = Path(here)
    legacy_insert = here / "insert_state.json"
    legacy_sync = here / "sync_state.json"
    legacy_extract = here / "extractions"
    if not legacy_insert.exists() and not legacy_sync.exists() and not legacy_extract.is_dir():
        return []
    notes: list = []
    lock = here / ".state_migrate.lock"
    try:
        here.mkdir(parents=True, exist_ok=True)
        fh = open(lock, "a+")
    except OSError:
        return []
    try:
        fcntl.flock(fh, fcntl.LOCK_EX)
        # Re-check under the lock: another process may have just finished.
        if not legacy_insert.exists() and not legacy_sync.exists() and not legacy_extract.is_dir():
            return []

        done_map, entities = {}, {}
        if legacy_insert.exists():
            try:
                data = json.loads(legacy_insert.read_text() or "{}")
                done_map = data.get("done", {}) or {}
                entities = data.get("entities", {}) or {}
            except Exception:
                done_map, entities = {}, {}
        sync_map = {}
        if legacy_sync.exists():
            try:
                sync_map = json.loads(legacy_sync.read_text() or "{}") or {}
            except Exception:
                sync_map = {}

        slugs = _store_slugs()
        per: dict = {}          # slug -> {"done": {...}, "sync": {...}}
        orphans = 0
        for fname, uuid in done_map.items():
            owner = _owner_slug(fname, sync_map.get(fname), slugs)
            if owner is None:
                orphans += 1
                continue
            d = per.setdefault(owner, {"done": {}, "sync": {}})
            d["done"][fname] = uuid
            if fname in sync_map:
                d["sync"][fname] = sync_map[fname]

        # The entity name→uuid map is global today (built before tenancy). Keep it
        # with the store that owns the most done files — historically the single
        # pre-tenancy store — so its future inserts still reuse those uuids; every
        # other store starts empty and builds its own per-group map as it inserts.
        main = max(per, key=lambda s: len(per[s]["done"]), default=None)
        for slug, d in per.items():
            sd = state_dir(here, slug)
            (sd / "extractions").mkdir(parents=True, exist_ok=True)
            ins = {"done": d["done"], "entities": entities if slug == main else {}}
            (sd / "insert_state.json").write_text(json.dumps(ins, indent=1))
            (sd / "sync_state.json").write_text(json.dumps(d["sync"], indent=1, sort_keys=True))
            notes.append(f"{slug}: {len(d['done'])} done migrated")

        moved = 0
        if legacy_extract.is_dir():
            for jf in sorted(legacy_extract.glob("*.json")):
                try:
                    f = json.loads(jf.read_text()).get("file") or (jf.stem + ".md")
                except Exception:
                    f = jf.stem + ".md"
                owner = _owner_slug(f, sync_map.get(f), slugs)
                if not owner:
                    continue
                dest = state_dir(here, owner) / "extractions"
                dest.mkdir(parents=True, exist_ok=True)
                try:
                    jf.replace(dest / jf.name)
                    moved += 1
                except OSError:
                    pass

        # Retire the flat files so this never runs again (kept, not deleted, so an
        # operator can inspect what was migrated).
        if legacy_insert.exists():
            legacy_insert.replace(here / "insert_state.json.migrated")
        if legacy_sync.exists():
            legacy_sync.replace(here / "sync_state.json.migrated")
        if legacy_extract.is_dir():
            try:
                legacy_extract.replace(here / "extractions.migrated")
            except OSError:
                pass
        notes.append(f"orphans dropped: {orphans}; extractions moved: {moved}")
        # This splits the on-disk INGEST STATE only. The Qdrant points and Graphiti
        # nodes still carry their pre-tenancy collection/group until `engram-tenant-
        # migrate` regroups them. Until that runs, historical episodes stay marked
        # "done" here (so `--insert` won't re-add them under the tenant group) yet
        # live in the old `canonical` group — invisible to the tenant's recall. On a
        # multi-tenant upgrade, run `engram-tenant-migrate` alongside/after this.
        if per:
            notes.append("NOTE: run `engram-tenant-migrate` to regroup Qdrant/Graphiti "
                         "data, or historical facts stay in the legacy group and recall "
                         "won't see them.")
        return notes
    finally:
        try:
            fcntl.flock(fh, fcntl.LOCK_UN)
        finally:
            fh.close()
