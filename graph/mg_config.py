"""mg_config.py — build a Graphiti instance wired to local infra only:
Neo4j on loopback; embeddings + reranking routed through engram_llm so the graph
works on EITHER backend (Ollama nomic, or CPU fastembed when there's no GPU).
Nothing leaves the machine. Neo4j password from env or the chmod-600 .env.
"""
import asyncio
import math
import os
import sys
from pathlib import Path

from graphiti_core import Graphiti
from graphiti_core.driver.neo4j_driver import Neo4jDriver
from graphiti_core.llm_client.openai_generic_client import OpenAIGenericClient
from graphiti_core.llm_client.config import LLMConfig
from graphiti_core.embedder.client import EmbedderClient
from graphiti_core.cross_encoder.client import CrossEncoderClient

HERE = Path(__file__).resolve().parent
# engram_llm lives in the engine dir: ../bin in the repo, ~/.claude once installed.
sys.path.insert(0, str(HERE.parent / "bin"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import engram_llm  # provider abstraction (ollama | claude); embed() = ollama nomic | fastembed

# LLM is only used by the (retired) Graphiti add_episode bootstrap path — insert/recall
# do not call it, so constructing it never reaches out. Kept for the bootstrap option.
OLLAMA_BASE = os.environ.get("OLLAMA_BASE_URL", "http://localhost:11434/v1")
LLM_MODEL = os.environ.get("MG_LLM_MODEL", "llama3.1:8b")
SMALL_MODEL = os.environ.get("MG_SMALL_MODEL", "llama3.1:8b")
NEO4J_URI = os.environ.get("NEO4J_URI", "bolt://127.0.0.1:7687")
NEO4J_USER = os.environ.get("NEO4J_USER", "neo4j")
CANONICAL_GROUP = "canonical"   # group_id for gate-approved memories


def neo4j_password() -> str:
    if os.environ.get("NEO4J_PASSWORD"):
        return os.environ["NEO4J_PASSWORD"]
    envf = HERE / ".env"
    if envf.exists():
        for line in envf.read_text().splitlines():
            if line.startswith("NEO4J_PASSWORD="):
                return line.split("=", 1)[1].strip()
    raise RuntimeError("NEO4J_PASSWORD not set (export it, or put it in graph/.env)")


def _cos(a, b) -> float:
    s = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a)) or 1.0
    nb = math.sqrt(sum(y * y for y in b)) or 1.0
    return s / (na * nb)


class EngramEmbedder(EmbedderClient):
    """Graphiti embedder backed by engram_llm.embed() — Ollama nomic when reachable,
    else CPU fastembed. 768-dim in both paths, so the graph never needs re-embedding
    when you switch backends."""
    async def create(self, input_data):
        text = input_data if isinstance(input_data, str) else " ".join(map(str, input_data))
        return await asyncio.to_thread(engram_llm.embed, text)

    async def create_batch(self, input_data_list):
        return [await asyncio.to_thread(engram_llm.embed, t) for t in input_data_list]


class EngramReranker(CrossEncoderClient):
    """Embedding-cosine reranker — needs no generation LLM, so recall works in
    claude-only mode (no Ollama) without calling out or spending tokens."""
    async def rank(self, query: str, passages):
        if not passages:
            return []
        qv = await asyncio.to_thread(engram_llm.embed, query)
        scored = []
        for p in passages:
            pv = await asyncio.to_thread(engram_llm.embed, p)
            scored.append((p, _cos(qv, pv)))
        scored.sort(key=lambda x: -x[1])
        return scored


def build_graphiti() -> Graphiti:
    llm = OpenAIGenericClient(
        config=LLMConfig(api_key="ollama", model=LLM_MODEL, small_model=SMALL_MODEL,
                         base_url=OLLAMA_BASE, temperature=0.0, max_tokens=8192))
    driver = Neo4jDriver(uri=NEO4J_URI, user=NEO4J_USER, password=neo4j_password())
    return Graphiti(graph_driver=driver, llm_client=llm,
                    embedder=EngramEmbedder(), cross_encoder=EngramReranker())
