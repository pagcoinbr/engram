---
name: acme-api service
description: AcmeCorp public REST API (FastAPI) on api-1 — auth, rate limits, Postgres + Redis backends, deployed via GitHub Actions.
type: project
---

## Summary
`acme-api` is AcmeCorp's public REST API (Python/FastAPI), running on host `api-1`
behind `gateway`. It authenticates with JWT, rate-limits per API key, persists to the
shared `postgres` database, and caches sessions in `redis`. Deploys run through GitHub
Actions on merge to `main`.

## Index
1. Stack & topology
2. Auth & limits
3. Deploy

## 1. Stack & topology
FastAPI app, runs as the `acme-api` systemd service on `api-1`. Fronted by `gateway`
(TLS termination + routing). Reads/writes `postgres`; uses `redis` for sessions + rate
counters. Talks to `billing-service` over the internal network for invoice creation.

## 2. Auth & limits
JWT bearer auth (15-min access tokens, refresh in `redis`). Per-API-key rate limit
(token bucket in `redis`, 600 req/min default). Returns `429` with `Retry-After`.

## 3. Deploy
`main` is production. CI builds a container, pushes to the registry, and the deploy job
restarts the `acme-api` service on `api-1`. See [[acme-api deploy procedure]]. Depends on
[[acme infrastructure]], [[postgres gotchas]], [[billing-service]].
