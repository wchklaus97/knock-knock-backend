# Knock Knock Release Verification Report

**Date:** 2026-08-10
**Scope:** current Phase 4/5 completion worktrees based on the merged checkpoint
and Phase 0–3 integration baseline
**Production changes:** none

## Completion branches

| Repository | Branch | Commit | Draft PR |
|---|---|---|---|
| Backend base | `agent/phase45-completion-backend` | `2977322` | merged Phase 4/5 base |
| Backend follow-up | `agent/phase45-completion-backend` | `e93a677` | local checkpoint; PR not pushed |
| iOS | `agent/phase45-completion-ios` | `e31101c` | pending draft PR |

The branches are intentionally based on the merged Phase 0–3 integration
baseline. They are not merged, deployed, or applied to production
automatically; the paired review and release gates still require human
approval.

## Implemented baseline

- Canonical D01–D40 architecture decisions with a Chinese summary.
- OpenAPI 3.1 REST, SSE, error, pagination, and `CommandEnvelope v1` contract.
- Backend migrations 0003–0011 for commands, confirmation, messages,
  retrievals, phone changes, outbox, retention/deletion metadata, rate limits,
  compatibility-operation claim fencing, and durable vertical-action effects.
- Server-side command validation, action registry, idempotency, confirmation,
  undo/cancel routes, retryable unknown outcomes, and outbox execution boundary.
- Durable local reminder and draft effects, an internal queued message effect,
  and provider-idempotency records for the three release vertical actions.
- Additive command-list, pairing-status, push-dismiss, and rich action-descriptor
  contracts; race-safe pairing claim tokens and command cursor pagination.
- Explicit local/external/disabled provider modes, action feature flags,
  provider-attempt failure states, bounded stale-lease recovery, credential-key
  rejection in command arguments, and secret-only JWT/APNs signing material.
- Versioned, atomic, idempotent command Undo for local reminder/draft effects.
- Signed model descriptor endpoint and production fail-closed model
  configuration checks; the iOS target consumes the official LiteRT-LM 0.12
  C framework without the upstream unsafe SwiftPM linker flags.
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
- `cargo test -q` — 32 passed
- `cargo check --target wasm32-unknown-unknown -q` — passed
- `scripts/architecture-migration-smoke.sh` — passed
- `scripts/adversarial-data-smoke.sh` — passed for cross-user isolation,
  deleted-resource write barriers, message/retrieval tombstones, lease fencing,
  event idempotency gates, outbox lease recovery, and cursor scope
- `scripts/contract-schema-smoke.sh` — passed
- `scripts/phase45-release-gate.sh` — passed
- `scripts/contract-smoke.sh` against an isolated local Worker + local D1 —
  passed, including command list, pairing status, push dismissal, and the
  existing multi-turn session/action loop.
- Local `/__scheduled` Outbox smoke — passed for reminder, draft, and message;
  message result remained `queued` with `external_delivery: not_configured`.
- guarded event/outbox/confirmation SQL was prepared and executed against
  SQLite — passed
- `git diff --check` — passed

### iOS

- iOS Simulator `VoiceAgentBridgeTests` — 33 passed, 0 failed
- Generic unsigned Release iOS device build with iOS 15 deployment target — passed
- Full UI test target — compiled, but the three E2E tests stopped at the login
  screen because the deployed Worker/`needs_user` fixture was not configured
  for this local run; this is an explicit release gate, not a passing result.
- `git diff --check` — passed

## Review findings addressed

The adversarial review identified and the integrated branches addressed the
following high-risk issues: sibling migration drift, incomplete deleted-session
write barriers for sessions and commands, non-atomic event idempotency claims,
un-fenced compatibility-operation lease takeover, permanently stuck Outbox
leases, unverified rate-limit identity, discarded structured retry metadata,
incomplete local tombstone cleanup, a globally writable skill registry, an iOS
permanent-error retry loop, and non-additive checkpoint response requirements.

## Remaining release gates

These are deliberately not marked as passed:

- route-level D1/E2E smoke against deployed bindings;
- backend GitHub CI, which was still pending at report time;
- concrete provider-backed executors for reminder delivery and external
  send-message delivery, plus a reminder due-time scanner (the current
  implementation safely persists/queues local effects and reports external
  delivery as not configured);
- 20–100 example golden voice dataset, ≥95% accuracy evidence, and zero
  high-risk false execution evidence;
- physical iPhone 13 audio, memory, thermal, and crash testing;
- security review, formal breaking-contract diff, and production observability
  review;
- human approval for merging this follow-up PR, production migrations, APNs changes,
  and model rollout.

## Rollback

Do not merge the follow-up PR until the gates above are approved. Revert
`e93a677` (and the documentation handoff commit) or close the draft PR; no
production data or migration has been changed. Migrations 0010 and 0011 are
additive and require a separately approved rollback plan if applied remotely.
