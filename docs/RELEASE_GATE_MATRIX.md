# Backend Release Gate Matrix

This matrix is the handoff checklist for the voice-workflow completion branches
with tested implementation heads backend `6d98166f` and iOS `94669e9e`. A
local smoke is evidence for code behavior only; it does not substitute for a
deployed staging resource, a real provider, a physical iPhone, or human
approval.

| ID | Gate | Current status | Evidence or next action | Owner / dependency |
|---|---|---|---|---|
| RG-01 | Staging Worker + D1 + R2 route E2E | **Passed for `c83b04d`** | Protected staging deploy and contract workflows passed, followed by 20/20 read-only health probes. This does not deploy or validate the current voice branches. See `STAGING_VERIFICATION.md`. | Control Tower + Cloudflare/Supabase access |
| RG-02 | Paired PR review and CI | **Current voice PRs pending** | Backend and iOS voice changes must be committed as paired draft PRs, pass their repository CI, and receive human review. No automatic merge or deployment is allowed. | Human reviewer + paired agents |
| RG-03 | Production provider contract | **Local adapter passed; vendor pending** | Local mock verifies reminder delivery/cancel, scheduled cancellation recovery, timeout reconciliation, and message `accepted → delivered → sent`. Cancellation now requires an explicit terminal provider state and uses a durable per-operation fence; exhausted retryable outcomes remain `unknown` for scheduled reconciliation. Select a vendor, run sandbox tests, verify its idempotency/cancellation/status semantics, configure Wrangler secrets, then approve rollout. | Backend/Operations + provider credentials |
| RG-04 | Voice golden set and model gate | **Partial — 1B accepted, rollout pending** | Gemma 3 1B passed the 32-example semantic gate at 1.000 with zero high-risk false executions and command p95 1.546 seconds on iPhone 17 Pro Max. The 270M candidate is rejected: its best controlled result was 0.500, below the 0.950 threshold, so its acquisition path fails closed. Production trust-key approval, private-R2 publication, microphone/thermal UAT, and human rollout approval remain. Follow `VOICE_MODEL_RELEASE_RUNBOOK.md`. | Voice/Verification agent + operator-approved 1B release |
| RG-05 | Physical iPhone and APNs gate | **Partial** | The final simulator safety run completed 186 unit tests (183 passed, 3 optional skips) and 4 UI tests (3 passed, 1 opt-in physical skip), with zero failures. Gemma 3 1B passed the physical iPhone 17 Pro Max semantic and latency gate; iPhone 13 retains deterministic parsing because 1B is too slow and 270M is inaccurate. The signed Staging app installs and launches on both phones, and a read-only staging D1 aggregate confirms two valid physical APNs registrations under one user. Identifier-free wakes and cold-launch REST reconciliation are implemented. Real microphone/VAD/interruption/memory/thermal/crash execution, APNs delivery, true airplane-mode recovery, and simultaneous two-phone UI convergence remain separate gates. | iOS/Verification agent + approved model and two phones |
| RG-06 | Security and observability review | **Partial — fail-closed automation added, external controls pending** | Static/adversarial checks pass. Pairing uses high-entropy tokens; APNs excludes full voice text; production D1 backup is encrypted and now performs an upload/download/decrypt/schema round trip. Exact-version release and rollback workflows fail closed. Formal review, alert destinations, a successful remote backup/restore drill, and production secret inventory still remain. | Security/Operations reviewer |
| RG-07 | Human release approval | **Workflow and protected environment configured; credentials pending** | Manual release and rollback workflows require exact identifiers and approval phrases. The GitHub `production` environment requires the repository owner as reviewer and accepts deployments only from `main`. Production secrets/remaining variables, a successful remote backup drill, and explicit approval for migrations, provider/APNs flags, and model rollout remain required. Follow `PRODUCTION_RELEASE_RUNBOOK.md`. | Product owner / release approver |

## What is already verified locally

- `cargo fmt --all -- --check`
- `cargo test --all-targets` — 76 passed, 0 failed
- `cargo clippy --all-targets -- -D warnings`
- `cargo check --target wasm32-unknown-unknown`
- OpenAPI schema and breaking-compatibility smokes
- executable Rust dispatch ↔ OpenAPI route parity smoke — 48 operations
- migration, adversarial-data, provider-safety, and production-config smokes
- local contract smoke, including `/v1/health`, key rotation, skills, session detail/progress, R2 download/retention, and provider lifecycle smokes
- signed model-manifest/artifact release, authenticated private-R2 download,
  integrity/rollback tests, and 32-example fixture-structure validation
- request-ID propagation, Prometheus readiness gauges, trusted-edge identity, and stale cancellation lease recovery
- protected staging deploy and staging contract workflows at `c83b04d`
- exact-current iOS implementation tests on iPhone 13 Pro and iPhone 17 Pro Max
  — 126 passed, 0 failed, 1 intentionally skipped on each device
- read-only staging D1 registration metadata — two tokenized physical iOS
  devices under one user; this is not delivery or convergence evidence

## Release rule

Do not extend RG-01's pass to an un-deployed commit, and do not mark
RG-03–RG-07 passed from local mocks or simulator-only results. The paired voice
changes may be merged only after RG-02 review; production rollout still
requires explicit human approval and the external evidence above.

## CI/release gate entry points

The entry points below are intentionally separate so a missing local tool or a
failed external response cannot be mistaken for evidence from another gate.

| Entry point | Scope | Required evidence boundary |
|---|---|---|
| `scripts/phase45-release-gate.sh` | Static Rust, WASM, contract, migration, adversarial, configuration, backup-restore, syntax, and hygiene checks | No deployed API, provider, APNs, or iOS evidence |
| `scripts/local-contract-gate.sh` | Isolated local Worker, D1 migrations, REST contract, request/command/session/cursor/device checks, rate-limit response, and local R2 retention | Local-only D1/R2 state; no production endpoint |
| `scripts/provider-local-gate.sh` | Loopback provider boundary, provider readiness/metrics, redacted logs, idempotency, cancellation, status reconciliation, and lifecycle failures | Deterministic mock only; no vendor or credential evidence |
| `scripts/provider-observability-smoke.sh` | API/provider/APNs/model readiness gauges, validated/fallback request IDs, optional model descriptor correlation, and secret-shaped output rejection | Health/metrics only; it never prints readiness configuration values |
| `scripts/provider-lifecycle-smoke.sh` | Provider IDs, duplicate delivery suppression, nonzero/structured errors, cancellation mismatch, pending reconciliation, and missing-ID fail-closed behavior | Local or explicitly provisioned staging Worker; external vendor semantics remain pending |
| `scripts/staging-contract-gate.sh` | HTTPS staging health policy, Supabase auth, D1 REST contract/rate limit, R2 upload/download/retention/isolation, and disabled-effect policy | Requires independent staging Worker/D1/R2/Supabase resources; refuses production-looking URLs |

The iOS staging gate remains an external frontend-repository entry point. This
backend gate preserves the server-side staging URL and REST/SSE contract used by
the iOS build; it does not claim simulator, physical-device, voice, or APNs
delivery evidence. Run the frontend's pinned Xcode/iOS 15 staging checks
separately and attach their actual output to RG-05.

## Tool and environment prerequisites

Run `scripts/ci-prerequisites.sh <profile>` before invoking a gate directly.
Profiles are `static`, `dynamic`, `storage`, `health`, `staging`, `backup`, and
`ios`. The dynamic GitHub job installs `worker-build` `0.8.3` and Wrangler
`4.81.1` before checking the profile. The staging workflow installs the same
Wrangler version. Static gates use portable `grep`; no runner-installed
ripgrep command is required.

The remote staging gate additionally requires two pre-provisioned, non-
production Supabase UAT accounts (one for each isolation principal), an
independent staging D1 database, a private staging R2 bucket, and a
materialized staging Wrangler configuration. It uses login mode rather than
creating accounts during the run, avoiding provider email/signup rate limits.

Readiness is intentionally evidenced at the boundary rather than by exposing
configuration:

- API: `/health`, `/v1/health`, and the Rust/Worker Prometheus info gauge.
- D1: local migrations plus authenticated contract/session/command operations.
- R2: the R2 route smoke, including user namespace, shared-key retention, and
  object deletion checks.
- Supabase: the staging-only login/protected API/refresh/logout smoke.
- APNs: the health `apns_ready` value and metrics gauge; the agreed staging
  sandbox profile is `push_mode=both`, `apns_ready=true`, and
  `apns_production=false`. This is signing readiness only; production APNs
  readiness and real-device delivery still require separately approved
  evidence.
- Provider/model: provider and model gauges, optional model ID/manifest
  correlation, and provider lifecycle state transitions.

Correlation checks are explicit: validated and rejected `X-Request-ID` values,
command detail/list IDs, session/chat IDs and session list membership, command
and sync cursors, stable device registration IDs, optional model descriptor IDs,
and structured rate-limit `429` responses with `Retry-After` and request IDs.
Failure diagnostics pass through the repository redaction filter; raw bearer,
API-key-shaped, password, APNs, Supabase, and CI secret-shaped values are not
printed.

`PROVIDER_STRICT_RESOURCE_IDENTITY=true` is intentionally not enabled by the
default local gate on this `origin/main` base: the corresponding Rust action
effect owner still needs to add the cancellation provider-ID match and
message provider-ID presence guards. After that source-side patch lands, the
owner can enable the strict lifecycle mode in CI and retain the fail-closed
`provider_cancel_mismatch` / `provider_missing_id` assertions.

No migration is added by the CI/release gate work. Rollback is a code-only
revert or closing the draft PR; if a future release exposes a provider/APNs
flag, disable that flag and reconcile unknown work before any production
rollback or migration decision.
