# Memory → graph extraction spec (the LLM does this; quality matters)

You convert canonical memory `.md` files into structured entities + relationship
facts for a knowledge graph. **Faithfulness is paramount** (a wrong fact is worse
than none) — extract ONLY what the memory states; never invent.

## For each filename you are given
1. `Read` the memory file `<store>/<FILE>` (the store path is resolved by the caller;
   default `~/.claude/projects/<slug>/memory/`).
2. Ignore the YAML frontmatter (between the first two `---`) for facts, but use its
   `name`/`description` as context. Extract from the body.
3. Produce one JSON object and `Write` it to the extractions dir as `<STEM>.json`
   (STEM = filename without `.md`).

## JSON schema
```json
{
  "file": "<FILE>.md",
  "entities": [ {"name": "...", "type": "<EntityType>"}, ... ],
  "edges": [ {"source": "<entity name>", "relation": "<REL>", "target": "<entity name>", "fact": "<one grounded sentence>"}, ... ]
}
```
- `source` and `target` of every edge MUST also appear in `entities`.
- 5–15 salient entities per memory (the important nouns), not every token. Quality over quantity.
- `fact`: ONE concise sentence, grounded in the memory, **preserving exact identifiers verbatim**
  (IPs, ports, paths, hostnames, service/unit names, amounts, dates, ids). If unsure, omit it.

## EntityType (controlled)
Server, Service, Project, Product, Wallet, Asset, Protocol, Tool, Network, Site,
Endpoint, Path, Repo, Person, Concept, Threat. (Use `Concept` if nothing fits.)

## Relation (controlled; SCREAMING_SNAKE; add a new one only if none fit)
RUNS, RUNS_ON, HOSTS, USES, DEPENDS_ON, PART_OF, SUPERSEDES, REPLACES, RELATES_TO,
CONNECTS_TO, FORWARDS_TO, LOCATED_AT, OWNS, PAYS_VIA, MANAGES, CONFIGURED_BY,
COMPROMISED_BY, GENERATES, USED_FOR, INTEGRATES_WITH, STORES, DEPLOYED_ON,
AUTHENTICATES_WITH, FUNDS, SWAPS_VIA, ROUTES_THROUGH.

## Canonical naming — USE THE SAME NAME for the same thing across memories
(this is how the graph links memories; consistency is the single most important rule)
Pick one stable token per real thing and reuse it everywhere. Use the clearest stable
identifier from your own memories — lowercase for hosts/services, the exact string for
IPs/ports/paths, the product's real name for projects. Illustrative examples (replace
with your own):
- **Servers/hosts** (bare lowercase hostname): `web-1`, `api-1`, `db-1`, `gateway`;
  a cloud VM with no hostname → its IP, e.g. `203.0.113.10`.
- **Services/units/processes** (their real name, lowercase): `api-service`, `worker`,
  `scheduler`, `nginx.service`, `postgres`, `redis`.
- **Products/projects** (as named): e.g. `AcmeApp`, `Billing`, `acme-monorepo`.
- **Protocols/tools** (real public names are fine): `Docker`, `Neo4j`, `Ollama`,
  `Postgres`, `Redis`, `Tailscale`, `GitHub`, `Stripe`.
- **Wallets/assets/data** (domain-specific identifiers as named): e.g. `prod-db`,
  `events-queue`, `BTC`, `USDC`.
- **People**: `user` (the operator) when the memory refers to the user.
For anything else, use the clearest stable identifier and keep it identical across files.

## Output
- One JSON file per memory, valid JSON, written to the extractions dir.
- Return ONLY a one-line summary: `done N files: <stems...>` (or note any you skipped and why).
