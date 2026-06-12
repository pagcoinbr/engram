"""vector_store.py — EngramVectorStore: a thin qdrant-client wrapper that indexes
the .md store for semantic recall + fast (ANN) dedup.

Design notes / lessons borrowed from the mem0 reference:
  • The collection is created with an explicit vector size (= engram_llm.embed_dim,
    768) and COSINE distance.
  • Each .md file maps to ONE point with a DETERMINISTIC id (uuid5 of slug+filename),
    so re-inserting upserts instead of duplicating, and delete-by-name is O(1).
  • The payload carries {file, name, description, type, slug, sha} for filtered
    search/list/delete and staleness detection.
  • The write path NEVER calls a collection-wide reset()/delete_all() — deletes are
    always per-point by id (only the explicit --rebuild drops the collection).

Embeddings come from engram_llm.embed() (Ollama nomic | fastembed). All methods
raise vector_config.VectorUnavailable when Qdrant is down, so callers can fall back
to the pure-markdown path.
"""
from __future__ import annotations
import os
import sys
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "bin"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import engram_llm
import memory_ai
import vector_config as vc

# Stable namespace so point ids are reproducible across machines/runs.
_NS = uuid.UUID("6f9b7c2e-2a4d-5e1f-9c3a-engram0vector".replace("engram0vector", "0a1b2c3d4e5f"))


def slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")


class EngramVectorStore:
    """Qdrant-backed semantic index over the markdown store. Construct lazily;
    construction itself does no network I/O beyond building the client object."""

    def __init__(self, cfg=None):
        self.cfg = cfg or memory_ai.load()
        self.client = vc.build_client(self.cfg)        # may raise VectorUnavailable
        self.collection = vc.collection_name(self.cfg)
        self.dim = vc.dim(self.cfg)

    # ---- collection lifecycle ------------------------------------------------
    def ensure_collection(self, *, recreate: bool = False) -> None:
        from qdrant_client import models as qm
        exists = self.client.collection_exists(self.collection)
        if exists and recreate:
            self.client.delete_collection(self.collection)
            exists = False
        if exists:
            # Guard against an embedding-model swap that changed the dim.
            info = self.client.get_collection(self.collection)
            cur = _existing_dim(info)
            if cur is not None and cur != self.dim:
                raise vc.VectorUnavailable(
                    f"collection '{self.collection}' has dim {cur} but embeddings are "
                    f"{self.dim}-dim — run `vector_sync.py --rebuild` to re-index.")
            return
        self.client.create_collection(
            collection_name=self.collection,
            vectors_config=qm.VectorParams(
                size=self.dim, distance=qm.Distance.COSINE, on_disk=vc.on_disk(self.cfg)),
        )

    # ---- ids -----------------------------------------------------------------
    def point_id(self, filename: str) -> str:
        return str(uuid.uuid5(_NS, f"{slug()}::{filename}"))

    # ---- writes --------------------------------------------------------------
    def upsert(self, *, filename: str, name: str, description: str,
               mtype: str = "", sha: str = "", vector=None) -> None:
        from qdrant_client import models as qm
        if vector is None:
            vector = engram_llm.embed(f"{name} {description}".strip(), self.cfg)
        payload = {"file": filename, "name": name, "description": description,
                   "type": mtype, "slug": slug(), "sha": sha}
        self.client.upsert(
            collection_name=self.collection,
            points=[qm.PointStruct(id=self.point_id(filename), vector=vector, payload=payload)],
        )

    def delete(self, filename: str) -> None:
        from qdrant_client import models as qm
        self.client.delete(
            collection_name=self.collection,
            points_selector=qm.PointIdsList(points=[self.point_id(filename)]),
        )

    # ---- reads ---------------------------------------------------------------
    def search(self, query: str, k: int = 6, threshold: float = 0.0) -> list[dict]:
        qv = engram_llm.embed(query, self.cfg)
        res = self.client.query_points(
            collection_name=self.collection, query=qv, limit=k,
            score_threshold=(threshold or None), with_payload=True).points
        return [self._hit(p) for p in res]

    def find_duplicates(self, threshold: float = 0.86, max_pairs: int = 30) -> list[tuple]:
        """ANN near-duplicate finder — replaces the O(n²) pairwise cosine loop.
        For each indexed point, ask Qdrant for its nearest neighbours and keep
        pairs scoring >= threshold (deduped, sorted high->low)."""
        seen = set()
        pairs = []
        for pt in self._scroll_all(with_vectors=True):
            f_a = (pt.payload or {}).get("file", str(pt.id))
            hits = self.client.query_points(
                collection_name=self.collection, query=pt.vector, limit=6,
                with_payload=True).points
            for h in hits:
                f_b = (h.payload or {}).get("file", str(h.id))
                if f_b == f_a or h.score < threshold:
                    continue
                key = tuple(sorted((f_a, f_b)))
                if key in seen:
                    continue
                seen.add(key)
                pairs.append((float(h.score), key[0], key[1]))
        pairs.sort(reverse=True)
        return pairs[:max_pairs]

    def list(self) -> list[dict]:
        return [self._hit(p) for p in self._scroll_all(with_vectors=False)]

    def stats(self) -> dict:
        cnt = self.client.count(collection_name=self.collection, exact=True).count
        return {"collection": self.collection, "points": cnt,
                "dim": self.dim, "on_disk": vc.on_disk(self.cfg)}

    # ---- helpers -------------------------------------------------------------
    def _scroll_all(self, *, with_vectors: bool):
        offset = None
        while True:
            points, offset = self.client.scroll(
                collection_name=self.collection, limit=256, offset=offset,
                with_payload=True, with_vectors=with_vectors)
            for p in points:
                yield p
            if offset is None:
                break

    @staticmethod
    def _hit(p) -> dict:
        pl = p.payload or {}
        return {"file": pl.get("file"), "name": pl.get("name"),
                "description": pl.get("description"), "type": pl.get("type"),
                "score": float(getattr(p, "score", 0.0) or 0.0)}


def _existing_dim(info):
    """Best-effort extraction of the configured vector size from a Qdrant
    get_collection() response (the nesting varies across client versions)."""
    try:
        vectors = info.config.params.vectors
        if hasattr(vectors, "size"):
            return int(vectors.size)
        if isinstance(vectors, dict):  # named vectors
            first = next(iter(vectors.values()))
            return int(getattr(first, "size", first.get("size")))
    except Exception:
        return None
    return None
