# engram vector index (Qdrant) — optional

A **Qdrant** vector store that indexes the markdown memory store for semantic recall
and fast (ANN) duplicate detection. It is **optional and off by default**: the `.md`
files remain the source of truth, and with the vector store disabled or Qdrant
unreachable engram falls back to pure markdown (the in-memory cosine duplicate finder
+ the Neo4j graph recall). Nothing breaks without it.

Embeddings reuse `engram_llm.embed()` (768-dim `nomic`, Ollama → fastembed fallback),
so the vector space matches the graph's exactly — no second embedding stack.

## Layout
- `vector_config.py` — resolve `engram.yaml vector_store.*` (+ env) and build a lazy
  `QdrantClient`. Raises `VectorUnavailable` when off/unreachable so callers fall back.
- `vector_store.py` — `EngramVectorStore`: ensure/upsert/search/find_duplicates/delete/
  list/stats. One point per `.md` (deterministic uuid5 id → upsert, not duplicate).
- `vector_sync.py` — sha-synced orchestrator: `--insert [--only F]` / `--rebuild` /
  `--delete F` / `--status`. No-op when disabled.
- `vector_mcp_server.py` — `engram-vector` MCP: `memory_vector_recall` /
  `memory_vector_search` / `memory_vector_stats`.
- `docker-compose.yml` — `qdrant/qdrant`, bound to `127.0.0.1:6333` only.
- `smoke_test.py` — end-to-end round-trip against a throwaway collection.

## Enable
```bash
./install.sh --vector            # builds vector/venv, registers the MCP, flips vector_store.enabled
cd ~/.claude/vector && docker compose up -d
~/.claude/vector/venv/bin/python ~/.claude/vector/vector_sync.py --rebuild   # seed from existing .md
```

## Operate
```bash
python vector_sync.py --status      # store memories / indexed / pending
python vector_sync.py --insert      # index new/changed memories (the daemon also does this)
python vector_sync.py --rebuild     # drop + re-embed everything (after an embedding-model change)
```

## Notes
- **Authority:** `.md`-authoritative. Qdrant is a *rebuildable* index — losing it never
  loses a memory; `--rebuild` regenerates it from the store.
- **Dim guard:** the collection is created at `engram_llm.embed_dim()` (768). If you
  swap embedding models, `ensure_collection` refuses a mismatched collection — run
  `--rebuild`.
- **Coexists with the graph:** Qdrant = dense semantic search + dedup; Neo4j/Graphiti =
  relational, multi-hop, temporal. Both index the same `.md` store.
