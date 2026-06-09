---
name: acme-api deploy procedure
description: How to deploy acme-api — merge to main triggers GitHub Actions; manual rollback by re-running the previous tag.
type: reference
---

## Summary
`acme-api` deploys via GitHub Actions on merge to `main`: build container → push to
registry → restart the `acme-api` service on `api-1`. Rollback is re-running the deploy
job for the previous image tag.

## Index
1. Deploy
2. Rollback

## 1. Deploy
Merge to `main` → CI `deploy.yml` builds + tags the image `acme-api:<sha>`, pushes it,
SSHes to `api-1`, pulls, and `systemctl restart acme-api`. Health check on `/healthz`
must return 200 within 30s or the job fails (and does NOT restart again).

## 2. Rollback
Re-run `deploy.yml` with `image_tag=<previous-sha>` (Actions → Run workflow). There are
no DB migrations in the deploy path; schema changes ship separately. Related:
[[acme-api service]], [[acme infrastructure]], [[verify before declaring done]].
