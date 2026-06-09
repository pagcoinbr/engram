---
name: developer profile
description: Who the operator is — a backend engineer on the AcmeCorp platform team; preferences and expertise.
type: user
---

## Summary
The operator is a backend engineer on the AcmeCorp platform team, primary owner of
`acme-api` and `billing-service`. Strong in Python/TypeScript and Postgres; security-
conscious; prefers small, reviewable changes and explicit error handling.

## Index
1. Role & ownership
2. Preferences

## 1. Role & ownership
Owns `acme-api` (the public REST API) and `billing-service`. Reviews most platform PRs.
Works primarily on `api-1` and the shared `postgres` instance.

## 2. Preferences
Prefers typed code, dependency injection, and tests that exercise real behavior over
mocks. Dislikes large unreviewable diffs. Wants commands delivered as runnable scripts.
Related: [[deliver runnable scripts]], [[verify before declaring done]].
