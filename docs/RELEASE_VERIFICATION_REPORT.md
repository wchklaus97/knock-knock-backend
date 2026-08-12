# Knock Knock Release Verification Report

**Date:** 2026-08-12
**Scope:** on-device voice workflow completion, command safety, signed model
supply chain, crash-safe command recovery, and paired iOS/backend UAT
**Production changes:** none; no deployment, remote migration, secret change,
APNs rollout, provider rollout, or model rollout was performed

## Completion branches

| Repository | Branch | Base commit | Tested implementation head | Draft PR |
|---|---|---|---|---|
| iOS | `agent/voice-goal-completion-ios-20260812` | `1757009` (merged frontend #15) | `d7d0390` after the #15 merge update | [frontend #17](https://github.com/wchklaus97/knock-knock-frontend/pull/17) |
| Backend | `main` plus `agent/reject-unqualified-270m-20260812` | `bbb4a82` (merged backend #29) | `bbb4a82` plus the model-policy follow-up | Follow-up PR pending creation |

Backend PR #29 and frontend PR #15 are merged. Frontend #17 remains the only
iOS voice PR to review; frontend #16 and backend #28 are superseded and must
not be merged. No production deployment or model rollout was performed.

## Superseding model and release-safety evidence

- Gemma 3 1B completed the checked-in 32-example semantic gate at 1.000
  accuracy with zero high-risk false executions. On iPhone 17 Pro Max its
  command p95 was 1.546 seconds, so it is the accepted model tier for that
  device class, pending publication and release approval.
- Gemma 3 270M initialized successfully, but scored 0.125 with the original
  prompt, 0.000 with unsupported constrained decoding, and at best 0.500 with
  the shortened prompt. It is rejected for iPhone 13, staging, and production.
- iPhone 13 remains on deterministic parsing plus clarification. The 1B tier
  was accurate there but missed the latency target with command p95 4.844
  seconds.
- Release builds now require a valid private public-key file path and matching
  private Info.plist. Missing, invalid, symlinked, or mismatched inputs fail
  the build. A downloaded model is never marked Ready until LiteRT-LM opens it;
  startup failure quarantines the candidate and restores a verified predecessor.
- The final iOS unit run completed 186 tests: 183 passed, 3 optional tests
  skipped, and 0 failed. The isolated local Worker/D1 UI suite completed 4
  tests: 3 passed, the opt-in physical voice test skipped, and 0 failed.

## Fifteen-step workflow evidence

| Step | Current evidence | Release status |
|---|---|---|
| 1. Hold push-to-talk | Production gesture wiring and controller lifecycle tests pass. | Implemented; real touch/microphone UAT open |
| 2. Foreground-only recording | Capture now synchronously rejects an already-inactive app and also aborts on later inactive/background transitions. | Implemented and unit-tested |
| 3. VAD speech/trailing silence | Sustained speech, trailing silence, no-speech, noise, and maximum-duration tests pass. | Implemented and unit-tested; acoustic tuning UAT open |
| 4. On-device STT | Apple Speech requires on-device recognition and has no cloud fallback. | Implemented; real microphone/transcription evidence open |
| 5. Local Gemma intent model | LiteRT-LM runtime, signed download, verification, activation, and rollback pass synthetic tests. | Runtime implemented; approved real model/key absent |
| 6. Strict iOS envelope validation | Strict JSON, duplicate-key rejection, allowlisted intent arguments, and app-owned policy tests pass. | Proven by tests |
| 7. Clarification | The production controller now maps low confidence, ambiguity, unsupported intent, and invalid model output to clarification without POST. | Proven by tests |
| 8. Backend submission | Crash-safe checkpoint precedes POST; exact idempotent replay and canonical GET reconciliation pass. | Proven by tests/local contract |
| 9. Independent backend validation | Auth, ownership, registry risk, confirmation, idempotency, and strict per-intent arguments are enforced at intake and revalidated before execution; cross-user reads and conflicting idempotency are route-smoked. | Proven by tests/local contract |
| 10. Read-only action | `search_history` is owner-scoped and executes without external side effects. | Proven locally; real voice entry open |
| 11. Reversible action/Undo | Reminder/draft Undo is authorized, idempotent, provider-fenced, advertised only while eligible, and limited to 600 seconds. | Proven locally; production provider semantics open |
| 12. One-time high-risk confirmation | Backend forces `send_message` to high risk, stores a token hash, increments command version while rotating exact replay, suppresses raced stale tokens, and atomically consumes once. | Proven by tests/local contract |
| 13. REST/SSE/APNs result path | REST/SSE reconciliation exists; supported Outbox terminal outcomes send a data-free best-effort APNs wake hint, and iOS cold-launch background delivery triggers REST refresh exactly once. Direct cancel/expiry still converge through sync/resume. | Implemented/tested; real APNs delivery open |
| 14. Backend-owned UI/TTS | iOS accepts only validated backend presentation and speaks backend `voice_script` once per version. | Proven by tests; audible real-device UAT open |
| 15. Raw-audio privacy | Audio buffers feed only on-device Speech and scalar RMS; no app upload or recording persistence API exists. | Source/test evidence; packet/filesystem UAT open |

## Implemented checklist

### iOS voice and command boundary

- [x] Push-to-talk capture with sustained-speech VAD, no-speech timeout,
  maximum-duration cutoff, and graceful final-transcript drain.
- [x] Capture, inference, and API submission are cancellable as a workflow;
  backgrounding, audio interruption, route loss, and media-service reset abort
  without automatic resume or command submission.
- [x] Audio category/options and TTS state are restored after capture.
- [x] Model output first decodes into an untrusted DTO. Duplicate JSON keys,
  unknown fields, aliases that collide, invalid types, unsupported intents,
  missing parameters, and out-of-policy lengths fail closed.
- [x] A local authoritative policy derives risk and confirmation requirements.
  The model cannot select identity, command ID, session, locale, timezone,
  model version, risk, or permission policy.
- [x] Supported release intents are exactly `search_history`,
  `create_reminder`, `create_draft`, and `send_message`; `send_message` is
  always high risk and always requires confirmation.
- [x] Low-confidence or ambiguous output produces clarification and never a
  guessed date, person, amount, or side effect.
- [x] Capture refuses to start if the application is already inactive, closing
  the lifecycle-notification race before the audio session is activated.

### Signed model supply chain

- [x] Domain-separated Ed25519 signing payload binds manifest schema, model
  ID/version, artifact SHA-256, byte size, and minimum capability.
- [x] Release script and Swift verifier use the same deterministic payload.
- [x] Descriptor ID and signed manifest ID must both match the requested
  `gemma-command` model.
- [x] Semantic-version precedence, active/rollback persistence, relaunch
  re-verification, streamed download, 2 GiB ceiling, exact-origin redirect
  fencing, HTTPS-only policy, hash/size/signature checks, and rollback are
  implemented.
- [x] LiteRT-LM creates a fresh conversation for each command and releases the
  response/conversation on every return path.
- [x] Model preparation is coalesced into one task and cancelled at logout.

### Crash-safe command lifecycle

- [x] SQLite stores a single active command checkpoint scoped by canonical API
  origin and authenticated stable user ID before the POST is issued.
- [x] Cold-start recovery reuses the exact envelope and idempotency key and
  rejects command-ID/version regressions.
- [x] Presentation text is backend-owned. Missing or invalid presentation is
  shown generically and is never synthesized from raw command arguments.
- [x] Terminal presentation persists until mounted and TTS speaks at most once
  for each backend command version.
- [x] Confirmation tokens are checkpointed before UI consumption.
- [x] If an awaiting-confirmation response is lost, an exact idempotent replay
  atomically invalidates the previous one-time token and returns a fresh token
  only after ownership, command hash, state, version, and expiry checks. Token
  rotation increments command version, and a response never returns a token
  made stale by a competing rotation. iOS also discards its older authority
  when it observes a newer awaiting-confirmation version without a token.

### Backend contract and privacy

- [x] Authenticated model descriptor and user-authorized private R2 streaming
  use `private, no-store` and do not expose internal R2 keys.
- [x] Command summaries and presentation responses omit raw command payload,
  result, error, recipient, and body data.
- [x] All four release intents use strict backend argument schemas that reject
  unknown keys, duplicate aliases, non-string values, missing fields, and
  unregistered intents.
- [x] Direct History search and `search_history` command arguments share the
  same trimmed 1–200-character contract across OpenAPI, Rust, and iOS.
- [x] Reversible success responses expose `undo_command_id` only inside a
  600-second window; replay after a completed Undo remains idempotent.
- [x] Supported Outbox success/failure/deleted-session terminal outcomes emit
  a best-effort silent APNs wake containing only `aps.content-available` and a
  fixed `wake_hint`; it carries no resource or business identifier. REST
  remains authoritative, including direct cancel/expiry reconciliation.
- [x] Registering an APNs token atomically transfers it away from rows owned by
  any prior account before associating it with the authenticated account.
- [x] Exact command replay and confirmation-token rotation are covered by Rust
  and local contract tests.
- [x] OpenAPI 3.1, production configuration checks, local Worker/D1/R2 gate,
  voice-model signing smoke, route parity, retention, isolation, correlation,
  rate limiting, provider lifecycle, and log sanitization are included in the
  release gate.

### UI regression harness

- [x] Each UI test creates its own authenticated local fixture.
- [x] Empty Xcode environment values no longer override the default local
  Worker URL; the same normalized URL is passed to the fixture process and the
  application process.
- [x] When a signed model trust key is intentionally absent, AppStore now maps
  the failure to an actionable configuration message and sanitizes every
  unknown model-preparation error. The mapping passes simulator and iPhone 17
  tests; final mirrored-banner visual confirmation waits for Mac unlock.
- [x] Permanent offline operation failures remain visible with Retry and
  Discard instead of being removed on the initial failed request.
- [x] A process-level dispatcher buffers command/session wake hints before any
  SwiftUI view appears, then triggers REST reconciliation and completes the
  UIKit fetch callback exactly once with `.newData`, `.noData`, or `.failed`
  according to the authenticated refresh result, including timeout races.
- [x] A manual retry requested during an active pending-operation pass queues a
  deterministic follow-up pass instead of being silently dropped.
- [x] Home Today/This Week, drawer, Settings/pairing, destructive confirmation,
  and queued-state flows pass against an isolated local Worker and local D1.

## Verification executed

### Backend

- `cargo fmt --all -- --check` — passed.
- `cargo test --all-targets` — 76 passed, 0 failed.
- `cargo clippy --all-targets -- -D warnings` — passed.
- `cargo check --target wasm32-unknown-unknown -q` — passed.
- `worker-build --release` — passed.
- `scripts/phase45-release-gate.sh` — passed.
- `scripts/local-contract-gate.sh` — passed against isolated Worker/D1/R2.
- OpenAPI route parity — 48 executable operations matched.
- Voice-model release/signature and authenticated R2 smokes — passed.
- Provider observability, rate-limit, lifecycle, production-config,
  adversarial data, backup/restore, retention, and log-sanitization gates —
  passed.
- Strict command isolation and conflicting-idempotency route smoke — passed.
- Staging health sampled 20 consecutive times — 20/20 passed. The deployed
  staging revision remains older than this unmerged branch.
- A 2026-08-12 read-only staging D1 aggregate found two distinct physical iOS
  registrations with valid 64-hex APNs tokens under one user. Their registration
  timestamps were `2026-08-11T18:44:18.224Z` and
  `2026-08-11T18:45:00.469Z`; no token, user ID, or device ID was read or
  printed. This proves same-account two-device registration only, not APNs
  delivery or UI convergence.
- Staging deploy/contract workflows now derive `SERVICE_VERSION` from the
  immutable `github.sha` and require health to match it; this provenance change
  has not been deployed.
- `git diff --check` — passed.

### iOS Simulator

- Full `VoiceAgentBridgeTests` on iPhone 15 / iOS 17.2 — 127 total:
  126 passed, 0 failed, 1 intentionally skipped.
- `VoiceAgentBridgeUITests` against isolated local Worker/D1 — 3 passed,
  0 failed.
- UI evidence captured for Home Today/This Week, drawer, Settings, decision
  detail, destructive confirmation, and queued state.
- Generic signed Release arm64 build with iOS 15 deployment floor — passed.
- `git diff --check` — passed.

### Physical iPhone 13 Pro

- Device: `Klaus 的iPhone`, iPhone 13 Pro, iOS 26.6 (`23G71`), wired developer
  mode.
- Staging configuration built and signed for `hk.knockknock.app` — passed.
- Staging endpoint embedded as
  `https://knock-knock-backend-staging.wch-klaus.workers.dev` — verified.
- App installed and launched while the device was unlocked — passed.
- Full `VoiceAgentBridgeTests` at iOS implementation head `94669e9e` — 127
  total: 126 passed, 0 failed, 1 intentionally skipped real-model gate.

### Physical iPhone 17 Pro Max

- Device: `Klaus’s iPhone 17 Pro Max`, iPhone 17 Pro Max, iOS 26.6 (`23G71`),
  paired Xcode device.
- Staging configuration built, signed, installed, and launched independently
  for `hk.knockknock.app` — passed.
- Full `VoiceAgentBridgeTests` at iOS implementation head `94669e9e` — 127
  total: 126 passed, 0 failed, 1 intentionally skipped real-model gate.
- Debug UI fixtures were intentionally not run on this phone because their
  isolation contract clears Keychain and local cache. This preserves the
  user's existing Staging login; the same UI workflows passed on the isolated
  simulator instead.

## Intentionally open release gates

The 1B model has local golden and physical-device performance evidence, but it
has not been published to staging R2 or approved for rollout. Normal automated
test runs therefore continue to skip the opt-in real-model test when its
private model, manifest, and trust-key paths are absent. That is the expected
fail-closed boundary.

- [ ] Approve, sign, and publish the already-qualified 1B `gemma-command`
  artifact to non-public staging R2, then repeat the golden gate from the exact
  staged descriptor and binary.
- [ ] Run real microphone → STT → Gemma → CommandEnvelope → backend → TTS UAT
  on iPhone 17 Pro Max, including Chinese, English, and Cantonese locale labels.
- [ ] Validate deterministic parsing plus clarification on iPhone 13 Pro; do
  not install the rejected 270M model there.
- [ ] Measure physical-device latency, peak memory, thermal state, battery
  impact, cancellation during inference, and repeated-session crash behavior.
- [ ] Verify real APNs delivery rather than configuration readiness only.
- [ ] Verify true airplane-mode cached display/recovery and simultaneous
  same-account convergence on two physical devices.
- [ ] Approve an external action provider and its credentials. Staging remains
  intentionally fail-closed while `action_provider_ready=false`.
- [ ] Complete paired human review, security/observability review, and explicit
  approvals for merge, production migration, APNs, provider, and model rollout.

LiteRT-LM 0.12 exposes a synchronous native send call. Task cancellation
prevents backend submission immediately, but the native call itself cannot be
preempted; its conversation is released when the call returns. This limitation
must be included in physical latency and lifecycle UAT.

## Release decision and rollback

The implementation is ready for paired draft PR review, not production
release. Rollback is to revert/close the paired PRs. No remote schema, object,
secret, APNs configuration, production provider, or production data was
changed by this work.
