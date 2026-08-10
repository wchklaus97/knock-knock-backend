# Knock Knock Release Verification Report

**Date:** 2026-08-10
**Scope:** current Phase 4/5 completion worktrees based on the merged checkpoint
and Phase 0–3 integration baseline
**Production changes:** none

## Completion branches

| Repository | Branch | Commit | Draft PR |
|---|---|---|---|
| Backend base | `main` | `3dcea11` | merged Phase 4/5 + contract parity |
| Backend follow-up | `agent/phase45-completion-backend` | `cccd12d` | [merged PR #11](https://github.com/wchklaus97/knock-knock-backend/pull/11) |
| Backend contract parity | `agent/contract-parity-backend` | `195ef2a` | [merged PR #12](https://github.com/wchklaus97/knock-knock-backend/pull/12) |
| iOS | `agent/phase45-completion-ios` | `e31101c` | pending draft PR |

The follow-up branch is based on merged PR #10. PR #11 passed its GitHub
Actions Rust backend CI run and is merged into `main`; PR #12 added the
47-operation route/OpenAPI parity and also passed CI before merging as
`3dcea11`. Neither change is deployed or applied to production. The remaining
release gates still require human approval. The numbered gate handoff is tracked in
[`docs/RELEASE_GATE_MATRIX.md`](RELEASE_GATE_MATRIX.md).

## Implemented baseline

- Canonical D01–D40 architecture decisions with a Chinese summary.
- OpenAPI 3.1 REST, SSE, error, pagination, and `CommandEnvelope v1` contract.
- Backend migrations 0003–0012 for commands, confirmation, messages,
  retrievals, phone changes, outbox, retention/deletion metadata, rate limits,
  compatibility-operation claim fencing, and durable vertical-action effects.
- Server-side command validation, action registry, idempotency, confirmation,
  undo/cancel routes, retryable unknown outcomes, and outbox execution boundary.
- Provider cancellation now requires an explicit terminal cancellation state
  and uses a durable per-operation idempotency fence; exhausted retryable
  outbox work remains `unknown` instead of being misreported as terminal
  failure, and scheduled reconciliation can complete pending cancellation
  without another user request.
- Durable local reminder and draft effects, an internal queued message effect,
  and provider-idempotency records for the three release vertical actions.
- Additive command-list, pairing-status, push-dismiss, and rich action-descriptor
  contracts; race-safe pairing claim tokens and command cursor pagination.
- Explicit local/external/disabled provider modes, action feature flags,
  provider-attempt failure states, bounded stale-lease recovery, credential-key
  rejection in command arguments, and secret-only JWT/APNs signing material.
- Non-development legacy JWT configuration fails closed without an explicit
  32-character secret; pairing codes use high-entropy URL-safe tokens with a
  tighter unauthenticated rate-limit bucket; APNs payloads contain only
  privacy-light identifiers rather than the full voice script.
- Request correlation accepts only validated `X-Request-ID` values, rate-limit
  and audit metadata use the trusted Cloudflare edge IP header, and `/metrics`
  exposes provider/APNs/model readiness gauges.
- Secret-authenticated HTTPS provider webhook adapter for reminders and
  messages, reminder due-time leases/retries, deduplicated reminder pushes,
  and scheduled message/retrieval retention sweep.
- Authenticated retrieval download streaming from R2 with user/session/expiry
  checks, private no-store response headers, no `r2_key` disclosure, and
  retention cleanup that removes only unreferenced R2 objects before deleting
  D1 metadata. New object references are restricted to the authenticated
  user's `users/{user_id}/retrievals/` namespace.
- Provider lifecycle operations for external reminders/messages: delivery,
  status lookup, reminder cancellation for Undo, timeout-to-unknown handling,
  and idempotent status reconciliation without a duplicate provider delivery;
  provider keys are user/action scoped, the running attempt is persisted before
  the provider call, and legacy keys remain reconcilable.
- Outbox idempotency keys are user/operation scoped while the original client
  `CommandEnvelope.idempotency_key` remains unchanged in the command resource.
- Local reminder stale leases are bounded by the attempt limit, and a deleted
  session cannot trigger a local due-time notification.
- Safe staging Wrangler template with explicit origin/version validation and
  disabled external effects.
- OpenAPI compatibility baseline and breaking-change smoke for retained v1
  routes, methods, required fields, enum values, and the actual `/health` route.
- Versioned, atomic, idempotent command Undo for local reminder/draft effects.
- Signed model descriptor endpoint and production fail-closed model
  configuration checks, including required manifest integrity/capability
  fields; the iOS target consumes the official LiteRT-LM 0.12
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
- `cargo test -q` — 42 passed
- `cargo check --target wasm32-unknown-unknown -q` — passed
- `worker-build --release` — passed; optimized Worker bundle generated
- `scripts/architecture-migration-smoke.sh` — passed
- `scripts/adversarial-data-smoke.sh` — passed for cross-user isolation,
  deleted-resource write barriers, message/retrieval tombstones, lease fencing,
  event idempotency gates, outbox lease recovery, and cursor scope
- `scripts/contract-schema-smoke.sh` — passed
- `scripts/contract-route-parity-smoke.sh` — passed: 47 executable operations match OpenAPI
- `scripts/contract-breaking-smoke.sh` — passed
- `scripts/provider-safety-smoke.sh` — passed
- `scripts/r2-download-smoke.sh` against an isolated local Worker + local D1/R2
  — passed for authorized streaming, metadata headers, no key disclosure,
  user-namespaced keys, shared-key retention cleanup, and cross-user isolation.
- `scripts/r2-download-smoke.sh` now supports the same route/retention/isolation
  flow against deployed staging with `R2_SMOKE_REMOTE=true` and a materialized
  staging Wrangler config; that external run remains pending.
- `scripts/provider-lifecycle-smoke.sh` against an isolated local Worker and
  mock provider — passed for reminder delivery/cancellation, scheduled
  cancellation recovery, timeout status reconciliation, and an asynchronous high-risk message moving from provider
  `accepted` to status `delivered` before the command became `sent`; provider
  keys remained user/action scoped and no duplicate delivery was observed.
- `scripts/provider-local-gate.sh` — passed; it created isolated
  local D1 persistence, started the deterministic mock boundary and an
  `external` Worker, then completed the full provider lifecycle smoke.
- `scripts/provider-observability-smoke.sh` — passed against that isolated
  Worker; readiness gauges, valid/invalid request-ID behavior, and generated
  fallback correlation IDs were verified, and the local provider token was
  absent from the Worker/provider logs.
- `scripts/production-config-smoke.sh` — passed, including the staging
  template and staging fail-closed checks
- `scripts/backup-restore-smoke.sh` — passed for encrypted export/decrypt,
  checksum, SQLite integrity, and schema/data restore
- `scripts/phase45-release-gate.sh` — passed
- [PR #11 GitHub Actions Rust backend CI](https://github.com/wchklaus97/knock-knock-backend/actions/runs/31347710519) — passed for final commit `5b7f59101745c7d3feae3c2c60175f0dc9e5ce35`, then merged as `cccd12d`
- [PR #12 GitHub Actions Rust backend CI](https://github.com/wchklaus97/knock-knock-backend/actions/runs/31348968440) — passed for commit `195ef2a`, then merged as `3dcea11`
- Read-only production health probe — passed; deployed version was
  `2026.08.08-build-25`, so this does not count as PR #11 deployment evidence.
- `scripts/staging-contract-gate.sh` and manual
  `.github/workflows/staging-contract-gate.yml` — prepared, not executed;
  the workflow now materializes a staging Wrangler config and includes the
  deployed R2 route smoke, but independent staging Worker/D1/R2 resources and
  UAT credentials do not yet exist.
- `scripts/contract-smoke.sh` against an isolated local Worker + local D1 —
  passed, including command list, pairing status, push dismissal, and the
  existing multi-turn session/action loop, metrics readiness gauges, and
  validated request-ID propagation.
- `scripts/local-contract-gate.sh` — passed; it started an isolated Worker and
  exercised the complete local contract, R2 download/retention, and ownership
  flow in one reproducible gate.
- Local `/__scheduled` Outbox smoke — passed for reminder, draft, and message;
  message result remained `queued` with `external_delivery: not_configured`.
- Local reminder due-time smoke — passed: a due reminder generated one
  deduplicated development push across repeated scheduled runs.
- Local external-provider smoke with a mock HTTPS-boundary adapter — passed
  for reminder delivery, provider cancellation, timeout reconciliation, and
  confirmed high-risk message delivery; the message remained `queued`/unknown
  while only accepted and became `sent` only after status reconciliation.
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
permanent-error retry loop, non-additive checkpoint response requirements,
cross-user Provider and Outbox idempotency-key collision risk, unscoped R2
references, shared-key retention deletion, and the provider-call crash window.
The latter issues are closed in PR #11 for new data; legacy records remain
reconcilable and still require staged migration evidence.

## Remaining release gates

The same gates are numbered RG-01–RG-07 in
[`docs/RELEASE_GATE_MATRIX.md`](RELEASE_GATE_MATRIX.md) so agents cannot
mistake local verification for production approval.

These are deliberately not marked as passed:

- independent staging Worker + D1 creation and route-level D1/E2E smoke
  plus R2 bucket creation and route-level D1/R2/E2E smoke against those
  deployed bindings;
- paired iOS/backend compatibility review after the merged contract-parity follow-up;
- production provider selection, vendor sandbox/contract evidence, real
  provider credentials, vendor-specific cancellation/reconciliation policy,
  and production rollout approval (the generic lifecycle adapter and a
  deterministic local subset of P01/P02/P04/P05/P06/P07/P08 are verified;
  P03/P09–P14 still require vendor evidence);
- 20–100 example golden voice dataset, ≥95% accuracy evidence, and zero
  high-risk false execution evidence;
- physical iPhone 13 audio, memory, thermal, crash, and real APNs testing;
- formal security review and production observability/alert review;
- human approval for production migrations, APNs changes,
  and model rollout.

## Rollback

Do not merge the follow-up PR until the gates above are approved. Revert the
follow-up commit (and its documentation handoff) or close the draft PR; no
production data or migration has been changed. Migrations 0010–0012 are
additive and require a separately approved rollback plan if applied remotely.
