# Memory Index

> Synthetic example memories shipped with engram (a fictional "AcmeCorp" stack).
> The installer seeds these only into an EMPTY store, so you can see the shape and
> watch the graph link them. Delete them once your own memories accrue.

## User & Feedback
- [developer profile](user_developer-profile.md) — who the operator is (backend eng, owns acme-api + billing-service)
- [verify before declaring done](feedback_verify-before-done.md) — run/test + show output before claiming done
- [deliver runnable scripts](feedback_deliver-runnable-scripts.md) — multi-step ops as an idempotent script on the target box

## Projects
- [acme-api service](project_acme-api.md) — public FastAPI on api-1 (JWT, rate limits, Postgres + Redis)
- [billing-service](project_billing-service.md) — Redis-queue worker, Stripe charges, idempotent on invoice id

## Reference
- [acme infrastructure](reference_acme-infra.md) — hosts: gateway / api-1 / db-1; private subnet + ports
- [acme-api deploy procedure](reference_acme-deploy.md) — merge-to-main GitHub Actions deploy + rollback
- [postgres gotchas](reference_postgres-gotchas.md) — connection cap, statement timeout, the charges unique index
