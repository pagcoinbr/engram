---
name: billing-service
description: AcmeCorp billing worker — consumes invoice jobs from Redis, charges via Stripe, writes to Postgres; idempotent on Stripe idempotency keys.
type: project
---

## Summary
`billing-service` is a background worker that consumes invoice jobs from a `redis`
queue, charges customers via `Stripe`, and records results in `postgres`. It is
idempotent: every charge uses a Stripe idempotency key derived from the invoice id, so a
retried job never double-charges.

## Index
1. Flow
2. Idempotency & failures

## 1. Flow
`acme-api` enqueues `invoice.create` jobs to `redis`. `billing-service` (on `api-1`)
pops a job, calls `Stripe` PaymentIntents, and on success writes a `charges` row to
`postgres` and emits `invoice.paid`.

## 2. Idempotency & failures
Idempotency key = `invoice:<id>`. On a transient Stripe error the job is requeued with
backoff; the key guarantees the retry reconciles to the same PaymentIntent rather than
creating a second charge. Related: [[acme-api service]], [[postgres gotchas]].
