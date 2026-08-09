# Knock Knock Release Verification Report

**Date:** 2026-08-09  
**Scope:** staged architecture implementation through draft PRs  
**Production changes:** none

## Integrated branches

| Repository | Branch | Commit | Draft PR |
|---|---|---|---|
| Backend | `agent/phase5-backend-release-integration` | `1eb452b` | [backend PR #8](https://github.com/wchklaus97/knock-knock-backend/pull/8) |
| iOS | `codex/phase5-ios-command-api` | `02141eb` | [iOS PR #7](https://github.com/wchklaus97/knock-knock-frontend/pull/7) |

The branches are intentionally stacked on earlier phase PRs. They are not
merged automatically; the checkpoint and phase dependency sequence still
requires human review.

## Implemented baseline

- Canonical D01–D40 architecture decisions with a Chinese summary.
- OpenAPI 3.1 REST, SSE, error, pagination, and `CommandEnvelope v1` contract.
- Backend migrations 0003–0009 for commands, confirmation, messages,
  retrievals, phone changes, outbox, retention/deletion metadata, rate limits,
  and compatibility-operation claim fencing.
- Server-side command validation, action registry, idempotency, confirmation,
  undo/cancel routes, retryable unknown outcomes, and outbox execution boundary.
- User-scoped history/retrieval/search/session/push routes and deletion
  tombstones.
- Cursor-based sync and notification-only SSE semantics.
- iOS 15-compatible SQLite cache, pending queue, cursor persistence, serialized
  SSE reconciliation, retry metadata handling, and message/retrieval tombstone
  convergence.
- Push-to-talk/VAD voice boundary, strict local command-envelope decoding,
  signed model manifest verification, and backend-only command submission.

## Verification executed

### Backend

- `cargo fmt --all -- --check` — passed
- `cargo clippy --all-targets -- -D warnings` — passed
- `cargo test -q` — 24 passed
- `cargo check --target wasm32-unknown-unknown -q` — passed
- `scripts/architecture-migration-smoke.sh` — passed
- `scripts/adversarial-data-smoke.sh` — passed for cross-user isolation,
  deleted-resource write barriers, message/retrieval tombstones, lease fencing,
  and cursor scope
- `scripts/contract-schema-smoke.sh` — passed
- guarded Outbox SQL was prepared and executed against SQLite — passed
- `git diff --check` — passed

### iOS

- iOS Simulator `VoiceAgentBridgeTests` — 30 passed, 0 failed
- Generic iOS build with iOS 15 deployment target — passed
- `git diff --check` — passed

## Review findings addressed

The adversarial review identified and the integrated branches addressed the
following high-risk issues: sibling migration drift, incomplete deleted-session
write barriers, un-fenced compatibility-operation lease takeover, unverified
rate-limit identity, discarded structured retry metadata, incomplete local
tombstone cleanup, and non-additive checkpoint response requirements.

## Remaining release gates

These are deliberately not marked as passed:

- route-level D1/E2E smoke against deployed bindings;
- backend GitHub CI, which was still pending at report time;
- provider-backed executors for reminder, draft, and send-message actions;
- 20–100 example golden voice dataset, ≥95% accuracy evidence, and zero
  high-risk false execution evidence;
- physical iPhone 13 audio, memory, thermal, and crash testing;
- security review, formal breaking-contract diff, and production observability
  review;
- human approval for merging stacked PRs, production migrations, APNs changes,
  and model rollout.

## Rollback

Do not merge the draft PRs until the gates above are approved. Revert the latest
integration commits (`1eb452b` and `02141eb`) or close the draft PRs; no
production data or migration has been changed. Migration 0009 is additive and
requires a separately approved rollback plan if it is ever applied.
