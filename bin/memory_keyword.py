#!/usr/bin/env python3
"""memory_keyword.py — pure-python BM25 lexical ranker over the .md memory store.

The lexical leg of hybrid recall: catches exact tokens (ports, ids, paths, flags)
that dense embeddings blur together — a query for `api-1` or `127.0.0.1:6333`
should land the file that literally contains it. Fused with graph + vector recall
via memory_fusion.

Zero dependencies (stdlib only) so it imports cleanly from BOTH the graph venv and
the vector venv — must never import qdrant_client/graphiti. Reads the store directly
(no embeddings, no services), so it is effectively always available.

CLI:
  memory_keyword.py "<query>" [--k N] [--type T]   # prints "file<TAB>score" lines
"""
from __future__ import annotations
import math
import os
import re
import sys
from pathlib import Path

HOME = Path.home()
# Tokens keep internal hyphens/dots/slashes/colons so ids/ports/paths survive whole
# (api-1, db-2, 127.0.0.1:6333, bolt://127.0.0.1:7687), then we ALSO emit the split
# parts so a query for `api` still matches `api-1`.
_TOKEN_RE = re.compile(r"[a-z0-9]+(?:[-_./:][a-z0-9]+)*")
_SPLIT_RE = re.compile(r"[-_./:]")
_K1 = 1.5
_B = 0.75


def slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")

def mem_dir() -> Path:
    return HOME / ".claude" / "projects" / slug() / "memory"


def _tokenize(text: str) -> list[str]:
    toks: list[str] = []
    for m in _TOKEN_RE.findall((text or "").lower()):
        toks.append(m)
        if _SPLIT_RE.search(m):           # api-1 -> also api, 1
            toks.extend(p for p in _SPLIT_RE.split(m) if p)
    return toks


def _frontmatter(p: Path) -> tuple[str, str, str]:
    """name / description / type from frontmatter (tolerant of the nested form)."""
    t = p.read_text(errors="ignore")
    def field(key):
        m = re.search(rf"^\s*{key}:\s*(.+)$", t, re.M)
        return m.group(1).strip().strip('"\'') if m else ""
    return field("name") or p.stem, field("description"), field("type")


def meta(filename: str) -> tuple[str, str, str]:
    """(name, description, type) for a store filename — for display of keyword-only
    hits in fused recall. Returns the stem as name if the file is missing."""
    p = mem_dir() / filename
    if not p.is_file():
        return filename.rsplit(".", 1)[0], "", ""
    return _frontmatter(p)


def _doc_tokens(p: Path) -> list[str]:
    """Field-weighted token bag: name x3, description x2, body x1 (TF weighting via
    repetition — simplest correct lever for BM25)."""
    text = p.read_text(errors="ignore")
    name, desc, _ = _frontmatter(p)
    body = re.sub(r"^---.*?---\s*", "", text, count=1, flags=re.S)  # strip frontmatter
    return _tokenize(name) * 3 + _tokenize(desc) * 2 + _tokenize(body)


def _corpus(mtype: str | None = None):
    d = mem_dir()
    if not d.is_dir():
        return []
    docs = []
    for p in sorted(d.glob("*.md")):
        if p.name == "MEMORY.md":
            continue
        if mtype:
            _, _, t = _frontmatter(p)
            if t != mtype:
                continue
        docs.append((p.name, _doc_tokens(p)))
    return docs


def rank(query: str, k: int = 6, mtype: str | None = None) -> list[tuple[str, float]]:
    """BM25 over the store. Returns [(file, score)] for positive-scoring docs,
    best-first, deterministic tie-break by filename. type-filtered when mtype set."""
    docs = _corpus(mtype)
    if not docs:
        return []
    N = len(docs)
    # document frequencies + per-doc term counts
    tf, df, dl = [], {}, []
    for _, toks in docs:
        counts: dict[str, int] = {}
        for w in toks:
            counts[w] = counts.get(w, 0) + 1
        tf.append(counts)
        dl.append(len(toks))
        for w in counts:
            df[w] = df.get(w, 0) + 1
    avgdl = (sum(dl) / N) or 1.0
    q_terms = set(_tokenize(query))

    scored = []
    for i, (fname, _) in enumerate(docs):
        s = 0.0
        for w in q_terms:
            f = tf[i].get(w, 0)
            if not f:
                continue
            # non-negative BM25+ idf. The 1+ form is provably >0 for df in [1,N]
            # (df is built fresh from this corpus, so df<=N); max() documents that
            # contract and guards any future formula edit.
            idf = max(0.0, math.log(1 + (N - df[w] + 0.5) / (df[w] + 0.5)))
            s += idf * (f * (_K1 + 1)) / (f + _K1 * (1 - _B + _B * dl[i] / avgdl))
        if s > 0:
            scored.append((fname, s))
    scored.sort(key=lambda x: (-x[1], x[0]))
    return scored[:k]


def main():
    args = sys.argv[1:]
    if not args:
        print('usage: memory_keyword.py "<query>" [--k N] [--type T]'); return
    k = int(args[args.index("--k") + 1]) if "--k" in args else 6
    mtype = args[args.index("--type") + 1] if "--type" in args else None
    skip = set()
    for flag in ("--k", "--type"):
        if flag in args:
            j = args.index(flag); skip.add(j); skip.add(j + 1)
    query = " ".join(a for i, a in enumerate(args) if i not in skip and not a.startswith("--"))
    for fname, score in rank(query, k, mtype):
        print(f"{fname}\t{score:.4f}")


if __name__ == "__main__":
    main()
