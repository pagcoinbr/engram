---
name: acme infrastructure
description: AcmeCorp host topology — api-1 (app), db-1 (Postgres+Redis), gateway (TLS/routing); private network + IPs.
type: reference
---

## Summary
AcmeCorp runs three hosts: `api-1` (application services), `db-1` (data stores), and
`gateway` (public edge). They share a private network; only `gateway` is internet-facing.

## Index
1. Hosts
2. Network

## 1. Hosts
- `gateway` — TLS termination + reverse proxy; the only public host. Forwards to `api-1`.
- `api-1` — runs `acme-api` and `billing-service` (systemd services).
- `db-1` — runs `postgres` (primary datastore) and `redis` (cache/queue).

## 2. Network
Private subnet `10.0.0.0/24`: `gateway` `10.0.0.2`, `api-1` `10.0.0.3`, `db-1` `10.0.0.4`.
`postgres` on `db-1:5432`, `redis` on `db-1:6379` — both bound to the private subnet only.
Related: [[acme-api service]], [[acme-api deploy procedure]].
