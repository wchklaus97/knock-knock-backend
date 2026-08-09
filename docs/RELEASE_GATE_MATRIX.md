# Backend Release Gate Matrix

This matrix is the handoff checklist for the Phase 4/5 completion branch. A
local smoke is evidence for code behavior only; it does not substitute for a
deployed staging resource, a real provider, a physical iPhone, or human
approval.

| ID | Gate | Current status | Evidence or next action | Owner / dependency |
|---|---|---|---|---|
| RG-01 | Staging Worker + D1 + R2 route E2E | **Pending external environment** | Local Worker/D1/R2 contract, R2 download, retention, and isolation smokes pass. Create independent staging resources, apply additive migrations, then run `scripts/staging-contract-gate.sh` and the deployed R2 route smoke. | Control Tower + Cloudflare/Supabase access |
| RG-02 | Paired PR review and CI | **CI passed; review pending** | PR #11 CI passed for the current branch. Review backend/iOS compatibility, migration IDs, and rollback notes before merge. | Human reviewer + paired iOS agent |
| RG-03 | Production provider contract | **Local adapter passed; vendor pending** | Local mock verifies reminder delivery/cancel, timeout reconciliation, and message `accepted → delivered → sent`. Select a vendor, run sandbox tests, verify cancellation/status semantics, configure Wrangler secrets, then approve rollout. | Backend/Operations + provider credentials |
| RG-04 | Voice golden set and model gate | **Pending** | Prepare 20–100 locale-tagged examples; require ≥95% intent accuracy, zero high-risk false executions, latency, memory, thermal, model signature, and rollback evidence. | Voice/Verification agent + signed artifacts |
| RG-05 | Physical iPhone and APNs gate | **Pending** | Run iPhone 13/real-device push-to-talk, interruption, memory, thermal, crash, APNs loss, and multi-device convergence tests. Simulator evidence is not sufficient. | iOS/Verification agent + Apple/APNs access |
| RG-06 | Security and observability review | **Pending formal review** | Static/adversarial checks pass. Review auth, provider secrets, R2 access, rate limits, redaction, tracing, metrics, alerts, unknown outcomes, and dead-letter/reconciliation runbooks. | Security/Operations reviewer |
| RG-07 | Human release approval | **Pending** | Record approval for PR merge, remote migrations, provider/APNs secrets, feature flags, and model rollout. Keep the rollback plan attached to the release record. | Product owner / release approver |

## What is already verified locally

- `cargo fmt --all -- --check`
- `cargo test --all-targets` — 39 tests
- `cargo clippy --all-targets -- -D warnings`
- `cargo check --target wasm32-unknown-unknown`
- OpenAPI schema and breaking-compatibility smokes
- migration, adversarial-data, provider-safety, and production-config smokes
- local contract, R2 download/retention, and provider lifecycle smokes
- PR #11 GitHub Actions Rust backend CI

## Release rule

Do not mark RG-01 or RG-03–RG-07 passed from local mocks or simulator-only
results. PR #11 may be merged only after RG-02 review and the release approver
accepts the external evidence or explicitly records a staged exception.
