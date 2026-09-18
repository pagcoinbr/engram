# Engram Memory Atlas

Maintained graph-first web UI for the local Engram installation. It inventories
every project store while labeling Neo4j coverage honestly.

## Build

```bash
npm install
npm run build
```

The service runs `atlas_api.py`, which loads the existing Engram API, adds the
Atlas endpoints, and serves `dist/`. Engram data stays under `~/.claude`.

Atlas complements the upstream terminal UI. It requires the graph installation
and is deployed separately with the included systemd service template.

## Graph semantics

- Solid arrows are explicit `[[wiki-links]]`.
- Dashed edges are shared extracted entities.
- Unknown coverage means the project has not been mapped to the active graph.
- Semantic similarity is not represented as evidence.

The backend binds to loopback and remains exposed through the existing Traefik
route at `https://engram.home.arpa`.
