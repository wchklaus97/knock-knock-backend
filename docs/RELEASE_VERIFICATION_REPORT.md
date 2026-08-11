# Knock Knock Release Verification Report

**Date:** 2026-08-12
**Scope:** on-device voice workflow completion, command safety, signed model
supply chain, crash-safe command recovery, and paired iOS/backend UAT
**Production changes:** none; no deployment, remote migration, secret change,
APNs rollout, provider rollout, or model rollout was performed

## Completion branches

| Repository | Branch | Base commit | Draft PR |
|---|---|---|---|
| iOS | `agent/voice-workflow-completion-ios-20260811` | `931c6bf54a328d067759daf1b243e75ae28bddcc` | [frontend #16](https://github.com/wchklaus97/knock-knock-frontend/pull/16) |
| Backend | `agent/voice-workflow-completion-backend-20260811` | `c83b04d6f71dbb0749f8dbaff641509b0d242f08` | [backend #28](https://github.com/wchklaus97/knock-knock-backend/pull/28) |

Both worktrees were fetched immediately before handoff and matched
`origin/main`. The changes remain unmerged until paired draft PR review.

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
  only after ownership, command hash, state, version, and expiry checks.

### Backend contract and privacy

- [x] Authenticated model descriptor and user-authorized private R2 streaming
  use `private, no-store` and do not expose internal R2 keys.
- [x] Command summaries and presentation responses omit raw command payload,
  result, error, recipient, and body data.
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
- [x] When a signed model trust key is intentionally absent, the physical app
  now shows an actionable configuration message instead of an internal Swift
  error type.
- [x] Home Today/This Week, drawer, Settings/pairing, destructive confirmation,
  and queued-state flows pass against an isolated local Worker and local D1.

## Verification executed

### Backend

- `cargo fmt --all -- --check` — passed.
- `cargo test -q` — 63 passed, 0 failed.
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
- `git diff --check` — passed.

### iOS Simulator

- Full `VoiceAgentBridgeTests` on iPhone 15 / iOS 17.2 — 114 total:
  113 passed, 0 failed, 1 intentionally skipped.
- `VoiceAgentBridgeUITests` against isolated local Worker/D1 — 3 passed,
  0 failed.
- UI evidence captured for Home Today/This Week, drawer, Settings, decision
  detail, destructive confirmation, and queued state.
- Generic signed Release arm64 build with iOS 15 deployment floor — passed.
- `git diff --check` — passed.

### Physical iPhone 13 Pro

- Device: `Klaus 的iPhone`, iPhone 13 Pro, iOS 26.6 beta, wired developer mode.
- Staging configuration built and signed for `hk.knockknock.app` — passed.
- Staging endpoint embedded as
  `https://knock-knock-backend-staging.wch-klaus.workers.dev` — verified.
- App installed and launched while the device was unlocked — passed.
- Full `VoiceAgentBridgeTests` on the physical device — 113 total:
  112 passed, 0 failed, 1 intentionally skipped.
- This run preceded the user-copy-only signed-model error follow-up. Its final
  114-test rerun is pending only because the device became locked while the
  owner was away.

### Physical iPhone 17 Pro Max

- Device: `Klaus’s iPhone 17 Pro Max`, iPhone 17 Pro Max, paired Xcode device.
- Staging configuration built, signed, installed, and launched independently
  for `hk.knockknock.app` — passed.
- Full `VoiceAgentBridgeTests` on the physical device at current PR head —
  114 total: 113 passed, 0 failed, 1 intentionally skipped.
- Debug UI fixtures were intentionally not run on this phone because their
  isolation contract clears Keychain and local cache. This preserves the
  user's existing Staging login; the same UI workflows passed on the isolated
  simulator instead.

## Intentionally open release gates

The skipped test is
`VoiceModelGoldenEvaluationTests.testSignedModelMeetsAccuracySafetyAndLatencyGates`.
It requires an approved real `.litertlm` Gemma artifact, a pinned production
public key, and signed descriptor inputs. No placeholder model is accepted.

- [ ] Approve and publish a real signed `gemma-command` artifact to non-public
  staging R2, then run the 20–100-example golden suite with at least 95%
  intent accuracy and zero high-risk false execution.
- [ ] Run real microphone → STT → Gemma → CommandEnvelope → backend → TTS UAT
  on iPhone 13 Pro, including Chinese, English, and Cantonese locale labels.
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
