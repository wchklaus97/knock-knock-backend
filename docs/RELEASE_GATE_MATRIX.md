# Backend Release Gate Matrix

This matrix is the handoff checklist for the post-PR #11 Phase 4/5 completion
and contract-parity branches. A
local smoke is evidence for code behavior only; it does not substitute for a
deployed staging resource, a real provider, a physical iPhone, or human
approval.

| ID | Gate | Current status | Evidence or next action | Owner / dependency |
|---|---|---|---|---|
| RG-01 | Staging Worker + D1 + R2 route E2E | **Pending external environment** | Local Worker/D1/R2 contract, R2 download, retention, and isolation smokes pass. `scripts/staging-contract-gate.sh` now includes the remote R2 upload/download/retention/isolation path; materialize a staging Wrangler config and run it against independent staging resources. | Control Tower + Cloudflare/Supabase access |
| RG-02 | Paired PR review and CI | **PR #11 merged; follow-up review pending** | PR #11 CI passed and was merged into `main`. Review the contract-parity follow-up for backend/iOS compatibility and route/schema parity before client regeneration. | Human reviewer + paired iOS agent |
| RG-03 | Production provider contract | **Local adapter passed; vendor pending** | Local mock verifies reminder delivery/cancel, scheduled cancellation recovery, timeout reconciliation, and message `accepted → delivered → sent`. Cancellation now requires an explicit terminal provider state and uses a durable per-operation fence; exhausted retryable outcomes remain `unknown` for scheduled reconciliation. Select a vendor, run sandbox tests, verify its idempotency/cancellation/status semantics, configure Wrangler secrets, then approve rollout. | Backend/Operations + provider credentials |
| RG-04 | Voice golden set and model gate | **Pending** | Prepare 20–100 locale-tagged examples; require ≥95% intent accuracy, zero high-risk false executions, latency, memory, thermal, model signature, and rollback evidence. | Voice/Verification agent + signed artifacts |
| RG-05 | Physical iPhone and APNs gate | **Pending** | Run iPhone 13/real-device push-to-talk, interruption, memory, thermal, crash, APNs loss, and multi-device convergence tests. Simulator evidence is not sufficient. | iOS/Verification agent + Apple/APNs access |
| RG-06 | Security and observability review | **Pending formal review** | Static/adversarial checks pass. Pairing now uses high-entropy tokens and a tighter unauthenticated bucket; non-development legacy JWTs require an explicit 32+ character secret; APNs excludes full voice text; production D1 backups are encrypted before private R2 storage; validated request correlation, trusted-edge rate-limit identity, readiness gauges, and stale cancel-lease recovery are implemented. Review auth revocation, provider secrets, R2 access, redaction, command/provider latency metrics, tracing, alerts, unknown outcomes, backup restore, and dead-letter/reconciliation runbooks. | Security/Operations reviewer |
| RG-07 | Human release approval | **Pending** | Record approval for PR merge, remote migrations, provider/APNs secrets, feature flags, and model rollout. Keep the rollback plan attached to the release record. | Product owner / release approver |

## What is already verified locally

- `cargo fmt --all -- --check`
- `cargo test --all-targets` — 42 tests
- `cargo clippy --all-targets -- -D warnings`
- `cargo check --target wasm32-unknown-unknown`
- OpenAPI schema and breaking-compatibility smokes
- executable Rust dispatch ↔ OpenAPI route parity smoke — 47 operations
- migration, adversarial-data, provider-safety, and production-config smokes
- local contract smoke, including `/v1/health`, key rotation, skills, session detail/progress, R2 download/retention, and provider lifecycle smokes
- local model-manifest shape/integrity validation and high-entropy pairing checks
- request-ID propagation, Prometheus readiness gauges, trusted-edge identity, and stale cancellation lease recovery
- PR #11 GitHub Actions Rust backend CI

## Release rule

Do not mark RG-01 or RG-03–RG-07 passed from local mocks or simulator-only
results. The post-PR #11 contract-parity change may be merged only after RG-02
review; production rollout still requires the release approver to accept the
external evidence or explicitly record a staged exception.

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
