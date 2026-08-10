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
