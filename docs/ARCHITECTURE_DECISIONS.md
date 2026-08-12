# Knock Knock Architecture Decisions

> **Status:** Accepted baseline (D01–D40)
>
> **中文摘要：** Knock Knock 采用 Backend 权威、REST + SSE + APNs、有限离线队列和版本化命令协议。iOS 负责本地语音理解和缓存，Backend 负责验证、执行、保存和同步。本文是两个仓库进行开发、review、迁移和发布时的唯一架构决策来源。

## Purpose and scope

Knock Knock is a decision channel for coding agents. The product loop is:

```text
Agent/MCP → Rust Cloudflare Worker → iPhone decision inbox
                                      ↓
                 local voice understanding → CommandEnvelope
                                      ↓
                 Backend validation → execution → history/sync
```

This document records the decisions approved during the architecture review. A
future change to an accepted decision requires an ADR update, a compatibility
note, tests, and a human-approved PR. It must not be silently changed inside a
feature PR.

## Baseline implementation gap

This is the gap analysis captured before the staged implementation began. The
checkpoint branches already contained authenticated Rust Worker APIs, D1
sessions/actions/audit/push records, foreground iOS SSE, REST reconciliation,
APNs integration, and a retry queue. The important gaps were:

- iOS pending operations are still persisted in `UserDefaults`, not the iOS 15
  compatible SQLite store.
- Backend SSE currently polls `sessions.updated_at`; the durable `phone_changes`
  cursor stream is not implemented.
- User-facing history currently maps `audit_logs`; `session_messages` and
  `retrieval_items` do not exist yet.
- Some business batches are followed by a separate audit write, so audit and
  state are not yet one atomic unit.
- The current `/reply` and `/confirm` routes predate the canonical command
  envelope and remain compatibility adapters during migration.

The current implementation status and remaining release gates are tracked in
`IMPLEMENTATION_ROADMAP.md`. The baseline gaps above remain useful because
they explain why each migration and compatibility adapter exists; they are not
permission to silently change an accepted decision inside a feature PR.

## Phase 4/5 follow-up status

The current completion branch adds durable vertical-action effects, the model
descriptor route, an iOS 15 push-to-talk/VAD controller, system on-device STT,
a LiteRT-LM 0.12 C-framework Gemma adapter, signed model
download/verification/rollback management, a 32-example multilingual golden
fixture, crash-safe SQLite command checkpoints, backend-owned UI/TTS
presentation, and a static release preflight. Exact idempotent replay of an
awaiting-confirmation command rotates its one-time token so a lost POST
response can recover without weakening confirmation.
The remaining gaps are intentionally operational: the official WhisperKit
package currently requires iOS 16, so it is not linked into the iOS 15 target;
the signed Gemma artifact and iOS public key must be supplied by the release
environment; and deployed D1/E2E, golden voice, physical-device, security,
provider rollout, and human release gates still require execution and approval.

## Current backend follow-up status — 2026-08-10

The current backend completion work has closed the following contract and
safety gaps without changing the REST + SSE baseline:

- `GET /v1/phone/commands` now provides user-scoped, cursor-paginated command
  summaries without confirmation tokens or full argument payloads.
- Pairing status, push dismissal, and persisted action descriptors are now
  available through additive routes/fields; pairing claims use a unique claim
  token to prevent same-timestamp double claims.
- Reminder and message effects now pass through an explicit provider mode and
  feature flags. Local development may persist D1 effects/queues; external
  mode uses a secret-authenticated HTTPS webhook with the command idempotency
  key and fails closed when an endpoint is not configured. An asynchronously
  accepted message remains queued/unknown until status reconciliation reports
  delivery, and an external reminder without a provider identifier cannot be
  reported as a successful undoable effect.
- Local reminders have a leaseable due-time scanner, retry state, provider
  identity, deduplicated push key, bounded stale-lease attempts, and a deleted
  session barrier. External reminders are excluded from that scanner so a
  provider-scheduled reminder is not delivered twice.
- A staging Wrangler template uses a separate D1/Supabase project, explicit
  origin/version checks, development push inbox, and disabled external effects.
  The canonical OpenAPI contract now includes the actual `/health` route and
  CI runs a compatibility baseline that rejects removed v1 operations or
  required fields.
- Outbox failures update provider attempt state, include 425 in retry handling,
  cap stale lease retries, reconcile pre-execution failures, and persist the
  provider running fence before an external call. Retryable exhaustion remains
  `unknown` for reconciliation rather than becoming a false terminal failure.
  Undo requires an explicit provider cancellation terminal state, uses a
  durable per-operation fence, and updates the command version and
  audit/change records atomically.
- Command arguments reject credential-shaped keys, while JWT and APNs signing
  material is read from Wrangler secrets rather than ordinary Worker vars.
- Retrieval snapshots now expose only a user-scoped `download_path`; the
  authenticated Worker streams the optional R2 object after checking the live
  session and retention window. New keys are restricted to
  `users/{user_id}/retrievals/`, downloads repeat that check, shared-key
  retention is reference-safe, the internal `r2_key` is never returned, and
  downloads use a dedicated rate-limit bucket plus `private, no-store`.
- External action configuration now requires delivery, status, and reminder
  cancellation endpoints before an enabled production action is considered
  ready. Provider timeouts are reconciled through the status endpoint, and
  external reminder Undo calls cancellation before changing local state.
  Cancellation is not treated as successful from HTTP 2xx alone.
  Accepted asynchronous message sends are not promoted to `sent` until status
  returns a terminal delivery state. Provider and Outbox idempotency keys are
  stable hashes scoped to user and operation, while the original command key
  stays in the command contract and legacy provider keys remain available for
  status reconciliation.

The remaining backend work is release evidence and deployment configuration,
not a new transport: create the independent staging D1/Worker and R2 bucket,
validate the chosen provider endpoints in their sandbox and configure their
production secrets, run remote D1/E2E, complete the formal security and
observability reviews, verify APNs on a real device, and obtain human approval
for migration/deployment/secret rollout. The PR #11 GitHub CI run is green.
The generic provider adapter, lifecycle reconciliation, R2 stream, local due
scanner, contract-breaking gate, retention sweep, and local dynamic smokes are
implemented and verified in this branch.

## Decision register

Each decision includes the approved behavior, why it matters, how it is tested,
and the current gap. IDs are stable and must not be reused.

| ID | Decision | Reason and implementation impact | Validation | Current gap |
|---|---|---|---|---|
| D01 | Backend is the only source of truth. | iOS may draft intent and cache data, but backend validates, executes, persists, and publishes state. | A local prediction never appears as committed without a backend result. | Canonical command submission and result routes exist; production/staging E2E and iOS UI proof remain. |
| D02 | Use risk classes: low, medium, high, destructive. | Read-only and reversible actions may run automatically; high-impact actions require confirmation. | Golden commands cover every risk class; high-risk false execution is zero. | Backend registry/confirmation policy and a 32-example structural fixture exist; real signed-model zero-false-execution evidence remains. |
| D03 | Separate sessions, messages, events, audit logs, retrievals, and R2 assets. | Each store has one purpose: state, conversation, domain events, security trail, sources, and large files. | History API returns messages, not audit rows; audit access is separately authorized. | D1 layers and user-facing APIs exist; retention/delete-all E2E and remote R2 lifecycle evidence remain. |
| D04 | SSE is notification-only; REST is the complete data path. | Small invalidations reduce traffic and make reconnect recovery deterministic. | Drop SSE frames, reconnect with cursor, then restore the same REST snapshot. | `phone_changes`, `/v1/phone/sync`, Last-Event-ID handling, and notification-only SSE exist; deployed multi-device gap-recovery evidence remains. |
| D05 | Use online-first with limited offline support. | Cached reads and safe queued commands are useful; arbitrary offline side effects are unsafe. | Offline UI shows pending, never success; only retryable commands are queued. | Backend command states, iOS SQLite cache/queue, and pre-POST command checkpointing exist; real airplane-mode UI E2E remains. |
| D06 | Offline voice may record, transcribe, and draft an intent; execution waits for backend. | Preserves privacy and low latency without violating backend authority. | Airplane-mode voice produces a draft/pending record and no server mutation. | Push-to-talk, VAD, Apple on-device STT, LiteRT-LM adapter, strict draft handling, and backend submission boundaries exist; Gemma 3 1B has physical-device semantic evidence, while real microphone and airplane-mode UAT remain. |
| D07 | All model output uses versioned `CommandEnvelope v1`. | Stable fields make local models replaceable and backend validation strict. | Schema tests reject missing version, intent, args, risk, or idempotency fields. | OpenAPI/JSON contract, Rust validation, strict iOS decoding, app-owned ID/locale/timezone/model canonicalization tests, and a 32-example real-model gate exist. Staged-artifact replay remains. |
| D08 | Commands use `pending → validated → awaiting_confirmation → queued → running → succeeded/failed/expired/cancelled`. | Explicit lifecycle prevents clients from guessing success and makes retries inspectable. | Every transition is authorized, persisted, and covered by transition tests. | Backend state machine, Outbox, unknown/retryable state, and local dynamic evidence exist; remote worker lease/incident evidence remains. |
| D09 | Confirmation is a one-time backend token bound to user, command hash, expiry, and command ID. | `confirmed: true` from a client is not an authorization decision. | Replayed, expired, changed, cross-user, or concurrently superseded tokens return a conflict and are never exposed as current. | Persistence, one-time use, expiry, concurrency checks, and audited token rotation exist. Every exact-replay rotation increments command version, a raced response with an older issued version withholds its stale token, and iOS discards prior local authority on a newer tokenless version; full high-risk UI E2E remains. |
| D10 | Raw audio is not uploaded or retained by default. | Text and command metadata are sufficient for product history and reduce privacy/storage risk. | Default trace contains no audio; opt-in audio expires and is deletable. | iOS voice boundary does not upload raw audio by default; an opt-in audio policy is intentionally not implemented. |
| D11 | Select models by runtime device capability, not only model name. | iPhone 13 may need a smaller tier; newer devices may use larger models without changing the protocol. A smaller model is never accepted only because it fits. | Model manifest, golden accuracy, and memory/thermal tests select a valid tier and support rollback. | Gemma 3 1B is accepted for iPhone 17 Pro Max. The tested 270M tier is rejected at 0.500 accuracy, so iPhone 13 keeps deterministic parsing plus clarification; production key/publication and thermal evidence remain. |
| D12 | Backend owns the action registry. | Action schemas, permissions, risk, and versions cannot be delegated to an LLM or client. | Unknown actions are rejected; client capability manifests never grant permission. | Backend registry/allowlist, strict per-intent argument validation at intake and execution, risk/confirmation override, and skill descriptors exist; a separately versioned public action-registry schema remains. |
| D13 | Every resource is scoped to authenticated user/account ownership. | Prevents cross-user reads and writes; identity comes from verified auth, not request JSON. | Isolation tests cover sessions, messages, retrievals, pushes, devices, and commands. | User-scoped queries and adversarial isolation checks exist; remote deployment and multi-device E2E remain. |
| D14 | Use deterministic cursor pagination, not large offsets. | `(created_at, id)` or a durable cursor avoids duplicates and slow deep pages on mobile. | Repeated page traversal has no duplicates or gaps under inserts. | Session, history, command, and sync cursors exist; deployed concurrent-insert traversal evidence remains. |
| D15 | Retrievals are immutable history snapshots. | Store title, URL, snippet, score, content hash, and optional R2 reference so past answers remain explainable. | A changed source does not alter a previously stored citation; authorized download respects retention and ownership. | Snapshot, user-namespaced R2 streaming, shared-key retention protection, and local evidence are implemented; staging/production R2 bucket provisioning and remote authorization evidence remain. |
| D16 | Lock the transport baseline to REST + SSE + APNs. | The product has server-to-phone notifications, not a required bidirectional stream; simpler transports are easier to recover. | Traffic and latency metrics are collected before any binary protocol decision. | Existing REST/SSE/APNs are present; MessagePack, WebSocket, and gRPC are intentionally not added. |
| D17 | Establish cross-repository contract tests before expanding models. | Deterministic fixtures catch schema drift without depending on model randomness. | Rust smoke, Swift decoding, SSE fixtures, and CI schema checks pass together. | OpenAPI, breaking-change, Rust, local Worker, and iOS decoding checks exist; paired cross-repository CI and full UI contract execution remain. |
| D18 | Use expand → migrate → contract. | Old clients and old routes remain valid while new fields and endpoints roll out. | A previous client fixture passes during every migration phase. | Additive migrations, compatibility adapters, and breaking-change smoke exist; remote migration/rollback evidence remains. |
| D19 | Prove the architecture with three vertical actions: read-only, reversible, high-risk-confirmed. | This exercises query, safe mutation/undo, and confirmation without implementing every feature first. | Each action completes voice/draft → backend → event → history → iOS UI. | Backend vertical paths, strict local envelope generation, backend presentation, Undo, token recovery, and real-model envelope evidence exist; real microphone-to-provider end-to-end proof remains. |
| D20 | First release gates are quantitative: ≥95% intent accuracy, 0 high-risk false executions, 0 duplicate side effects, and 1–2s local draft feedback. | Measurable gates prevent a demo from being mistaken for a reliable product. | Golden dataset, concurrency tests, latency metrics, and device thermal tests pass. | The 1B model passed 1.000 accuracy and zero high-risk false executions on iPhone 17 Pro Max. The 270M model was correctly rejected at a best 0.500 accuracy. Production rollout and thermal evidence remain. |
| D21 | Apply different retention policies to messages, retrievals, audio, audit, and deletion tombstones. | Product history needs persistence while raw audio and source payloads should expire. | Delete-all removes D1, R2, cache, and local data; tombstones prevent resurrection. | D1 retention fields, tombstones, scheduled cleanup, shared-R2 reference protection, and local evidence exist; remote retention, R2 lifecycle policy, and iOS cache deletion still require release verification. |
| D22 | Missing or ambiguous values require a follow-up question. | The model must not guess dates, people, amounts, permissions, or high-impact intent. | Fixtures with ambiguity produce clarification, never execution. | The production push-to-talk controller now maps low confidence/ambiguity to clarification without submission; prompt-injection fixtures pass. Real-model per-locale evidence remains. |
| D23 | Trace every command safely. | `command_id`, `session_id`, cursor, model version, latency, and final state enable diagnosis without sensitive logs. | Metrics and logs contain correlation IDs but no raw audio, token, or secret. | Request IDs are propagated only from validated `X-Request-ID` values or generated server-side; Prometheus readiness gauges, audit records, and structured errors exist. Full command/model/provider latency metrics, alerts, and redaction review remain. |
| D24 | Third-party credentials stay in backend with least privilege. | iOS, command JSON, SSE, history, and logs must never contain provider secrets. | Secret scanning and integration authorization tests pass; revocation works. | Secret-only provider tokens, request redaction, and local authorization smoke exist; vendor-specific least-privilege/revocation evidence remains. |
| D25 | Supabase Auth is the user identity authority. | One JWT/refresh lifecycle avoids a second incompatible user system. | Access, refresh, logout, expiry, and SSE reconnect tests use verified tokens. | Supabase adapter, verified user scope, refresh/logout paths, and local auth smoke exist; staging Supabase project and route-level evidence remain. |
| D26 | Support multiple devices from the first sync model. | Each device owns a cursor and APNs token; user data remains user-scoped. | Two-device update, reconnect, deletion, and account-switch tests converge without cross-user push ownership. | Device metadata, user-scoped sync cursors, tombstones, local multi-client routes, and atomic transfer of a reused APNs token away from prior-account rows exist; physical multi-device/APNs convergence remains. |
| D27 | APNs is a wake-up/reminder channel, never the data source. | Push loss is safe because REST sync recovers state; command wake payloads carry no resource or business identifier. | Drop push delivery and confirm foreground/resume sync produces the same state. | Supported Outbox terminal outcomes send only `aps.content-available` plus fixed `wake_hint=command`; iOS performs authenticated REST reconciliation. Direct cancel/expiry rely on SSE/resume sync. Real APNs delivery and loss/recovery UAT remain. |
| D28 | Commands within one session have backend ordering/version checks. | Clients cannot use wall-clock timestamps to resolve concurrent state changes. | Stale version returns conflict; concurrent same-session writes serialize. | Command versions, phone-change versions, and cursor ordering exist; multi-device concurrent session E2E remains. |
| D29 | State, domain event, audit, and phone change are committed atomically. | Clients must never be notified about a state that was not durably written. | Batch failure leaves no partial event or notification cursor. | Core command/event batches are atomic; some compatibility/business audit paths remain separate and require an outbox or explicit reliability review. |
| D30 | External side effects use an Outbox/Worker and provider idempotency. | Database transactions cannot atomically include email, messaging, payment, or provider APIs. | Timeout becomes `unknown/retryable`; accepted asynchronous delivery remains queued until status reconciliation; cancellation requires an explicit terminal provider state and repeated scheduled runs never duplicate a provider effect. | Outbox, pre-call running fence, user/action-scoped action keys, cancellation-operation fencing, conservative delivery/status/cancel adapter, and local async message/reminder mock evidence are implemented; a selected production vendor, sandbox proof, and durable cancel reconciliation policy remain. |
| D31 | First voice UX is push-to-talk with VAD end detection. | Avoids always-on microphone privacy, battery, and background complexity. | Permission, interruption, silence, cancel, and background transitions are tested. | iOS push-to-talk/VAD boundary exists; real-device interruption, thermal, and crash evidence remains. |
| D32 | Locale and timezone are explicit protocol metadata. | Backend must normalize dates, amounts, and names using device context. | Hong Kong Chinese/English fixtures parse against `Asia/Hong_Kong` deterministically. | `CommandEnvelope v1` requires locale/timezone and validation preserves them; full locale/clarification golden fixtures remain. |
| D33 | Rate-limit users, devices, SSE, commands, model fallback, and downloads. | Protects D1/AI cost and avoids reconnect storms. | Limits return stable error code and `retry_after`; normal usage remains unaffected. | User/device/SSE/command/model/download buckets are implemented; production thresholds and alert calibration remain. |
| D34 | Use one structured error envelope with retryability. | iOS can distinguish auth, conflict, expiry, validation, and network retry safely. | Client retries only retryable errors with backoff/jitter. | Versioned error envelope, retry metadata, and OpenAPI coverage exist; full cross-repository generated fixture and UI retry review remain. |
| D35 | Reversible actions return an explicit Undo command. | Compensating actions are safer than pretending an external transaction can roll back. | Undo is authorized, idempotent, time-bounded, and visibly fails when unavailable or when cancellation remains pending/unknown. | Reminder/draft responses advertise the original command ID only during a 600-second Undo window; authorization, idempotent replay, external cancellation fencing, and expiry tests pass. Production vendor semantics remain. |
| D36 | Retrieval content is untrusted data. | Web/file text cannot rewrite policy, permissions, or action instructions. | Prompt-injection fixtures never create an unauthorized action. | Retrieval snapshots are stored as data and never authorize commands; a formal prompt-injection golden suite and model retrieval path remain. |
| D37 | Models and model configuration use signed manifests. | Prevents arbitrary model downloads and supports safe rollback. | Hash/signature, minimum capability, expiry, and rollback tests pass. | Ed25519 release tooling, private-R2 authenticated streaming, Wi-Fi-safe disk download, hash/size/signature/capability checks, and rollback tests exist; an operator-approved signed Gemma artifact/key and rollout evidence remain. |
| D38 | Foreground owns SSE; background uses APNs and resume sync. | Matches iOS power constraints and preserves recoverability after termination. | Foreground/background/terminated flows restore cursor and state. | SSE resume, REST sync, persisted cursor, and a process-level dispatcher bound during `AppStore` initialization handle silent wakes without depending on `View.onAppear`; fetch completion reports data/no-data/failure exactly once. Real terminated/multi-device E2E remains. |
| D39 | iOS persistence uses system SQLite. | The app has an iOS 15 floor; SwiftData cannot be the required store. | Migration, crash recovery, logout deletion, and offline reads pass on iOS 15. | iOS 15 SQLite cache/queue plus scoped, version-checked active-command checkpoints, cold-launch reconciliation, and queued manual retry during active auto-retry exist; real termination/offline UI UAT remains. |
| D40 | Backend contracts are the single API schema source. | OpenAPI 3.1 plus embedded JSON Schema prevents iOS/backend field drift. | Schema lint, fixture validation, breaking-change CI, and generated examples pass. | OpenAPI covers lightweight command presentation, private model delivery, and all 48 routes; paired voice PR review and deployed compatibility smoke remain. |

## Canonical data model target

The target model is intentionally additive:

```text
sessions          current materialized state
session_messages  user-visible conversation
events            domain events and replay/audit input
audit_logs        security/administrative trail
commands          validated user intent lifecycle
action_attempts   external execution attempts and unknown outcomes
outbox_events     post-commit delivery work
phone_changes     durable per-user sync cursor
retrieval_items   source snapshots and R2 references
devices           per-device locale, push token, and sync metadata
pushes            wake-up history plus read/dismiss state
R2                audio and large retrieval payloads with expiry
```

All new rows carry the authenticated user scope where applicable. Existing
routes remain compatibility adapters until the new clients have migrated.

## Review and change control

Every implementation PR must link the affected decision IDs, contract diff,
tests, migration number, compatibility behavior, and rollback path. A change
to D01–D40 needs a new ADR section or an explicit superseding decision; it may
not silently edit the decision register.
