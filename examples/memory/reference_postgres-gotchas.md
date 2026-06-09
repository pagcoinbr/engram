---
name: postgres gotchas
description: Non-obvious facts about the shared Postgres on db-1 — connection cap, statement timeout, the charges unique index billing-service relies on.
type: reference
---

## Summary
The shared `postgres` on `db-1` has a low connection cap and a statement timeout that
have bitten us; `billing-service` also depends on a unique index that enforces charge
idempotency at the DB layer.

## Index
1. Limits
2. The charges unique index

## 1. Limits
`max_connections = 100` and both services share it — use a pooler (each service caps its
pool at 20). `statement_timeout = 10s`: long analytics queries get killed; run those on a
read replica, not the primary.

## 2. The charges unique index
`charges` has `UNIQUE (invoice_id)`. `billing-service` relies on this as the last line of
idempotency defense — an `ON CONFLICT (invoice_id) DO NOTHING` insert means a double-
processed job can't create a second charge even if the Stripe key check is bypassed.
Related: [[billing-service]], [[acme-api service]].
