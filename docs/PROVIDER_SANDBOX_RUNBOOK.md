# Provider Sandbox Runbook

> 中文摘要：本手册定义 Knock Knock 与外部提醒/消息供应商之间的最小
> HTTPS 契约，以及进入 staging/production 前必须完成的 sandbox 验证。
> 当前仓库只有通用 adapter 和本地 mock 证据；在选择供应商并取得其
> sandbox 凭证前，不得把 RG-03 标记为通过。

## Purpose

The backend owns the command state machine, user authorization, durable
idempotency, retries, and reconciliation. A provider is an external side-effect
boundary. It must not become the source of truth for command ownership or
client-visible authorization.

The current adapter is intentionally vendor-neutral. It uses authenticated
POST requests and supports three operations:

- delivery for `create_reminder` and `send_message`;
- status lookup for both actions;
- cancellation for the reversible reminder action.

The adapter is implemented in `src/providers.rs`; the end-to-end local
verification is `scripts/provider-lifecycle-smoke.sh`.

## Required environment configuration

Only the following non-secret values belong in Wrangler configuration:

| Variable | Required when | Meaning |
|---|---|---|
| `ACTION_PROVIDER_MODE` | all environments | `internal`, `external`, or `disabled` |
| `ACTION_REMINDER_ENABLED` | all environments | Enables reminder effects |
| `ACTION_MESSAGE_ENABLED` | all environments | Enables message effects |
| `ACTION_REMINDER_URL` | external reminder | Delivery endpoint |
| `ACTION_REMINDER_STATUS_URL` | external reminder | Authoritative status endpoint |
| `ACTION_REMINDER_CANCEL_URL` | external reminder | Reversible cancellation endpoint |
| `ACTION_MESSAGE_URL` | external message | Delivery endpoint |
| `ACTION_MESSAGE_STATUS_URL` | external message | Authoritative delivery endpoint |

The following values must be Wrangler secrets, never committed, returned in
API responses, or written to logs:

- `ACTION_REMINDER_TOKEN`
- `ACTION_MESSAGE_TOKEN`

External mode fails closed unless every enabled action has the required
endpoint and secret. Production must keep both action flags disabled until the
vendor sandbox evidence is reviewed.

## HTTP contract

Every provider request is `POST` with:

```http
Accept: application/json
Content-Type: application/json
Authorization: Bearer <Wrangler secret>
X-Idempotency-Key: kk_<stable user/action scoped hash>
X-Knock-Knock-Intent: create_reminder | send_message
```

The provider must treat `X-Idempotency-Key` as durable for the provider's
documented replay window. A repeated request with the same key must return the
same provider resource, or an equivalent already-known result, without
creating a duplicate side effect.

The JSON body is action-specific and may contain command arguments plus the
original command idempotency key for provider-side audit. The provider must
not require the mobile app to hold a provider credential.

### Delivery response

The response must be a 2xx JSON object and include a stable identifier under
one of the following keys:

```json
{
  "provider_id": "provider-resource-123",
  "state": "accepted"
}
```

Accepted/queued/processing results are not final message delivery. A message
remains queued/unknown until the status endpoint reports a terminal delivered
or sent state. A reminder is considered successful when the provider has
created a scheduled provider resource and returns its identifier.

Recognized state families are:

| Provider state family | Backend meaning |
|---|---|
| `accepted`, `queued`, `processing`, `running` | pending; reconcile later |
| `scheduled` for reminders | succeeded; provider resource exists |
| `sent`, `delivered`, `completed`, `succeeded` | succeeded |
| `failed`, `rejected`, `expired` | terminal failure |
| unknown value or missing state | unknown; never claim success |

### Status response

The status endpoint receives the same authentication, intent, idempotency
header, and provider payload context. It must return a stable provider ID and
one of `state`, `status`, or `delivery_state`:

```json
{
  "provider_id": "provider-resource-123",
  "delivery_state": "delivered"
}
```

The status response is authoritative after a timeout or Worker restart. The
backend retries only according to the durable outbox schedule and never sends
a second provider effect merely because the first response was lost.

### Reminder cancellation response

Cancellation is allowed only for a reversible reminder. The response must
explicitly prove a terminal cancellation:

```json
{
  "provider_id": "provider-resource-123",
  "state": "cancelled"
}
```

`cancelled`, `canceled`, `deleted`, or `removed` are terminal
cancellation states. Pending or missing cancellation state keeps Undo pending
and must not update the local reminder to `cancelled`.

## Sandbox verification matrix

Record the vendor name, sandbox base URL, API version, test date, request IDs,
provider resource IDs, and a redacted response fixture for every row.

| ID | Test | Expected result |
|---|---|---|
| P01 | Basic reminder delivery | one scheduled provider resource; command succeeds |
| P02 | Duplicate reminder delivery | same provider resource; no duplicate effect |
| P03 | Reminder status lookup | authoritative scheduled/terminal state is returned |
| P04 | Reminder Undo | only an explicit terminal cancel marks local state cancelled |
| P05 | Pending cancellation | API returns `provider_cancel_pending`; scheduled retry can finish it |
| P06 | Lost delivery response | command becomes `unknown`; status reconciliation settles it |
| P07 | Async message delivery | `accepted` stays pending until `delivered`/`sent` |
| P08 | Duplicate async message | status reconciliation does not send a second message |
| P09 | Provider 401/403 | fail closed; secret is not exposed; no retry storm |
| P10 | Provider 408/429/5xx | retryable unknown state with bounded backoff |
| P11 | Provider 4xx validation error | terminal failure with generic client error |
| P12 | Cross-user replay | a key or provider ID from another user cannot alter local state |
| P13 | Worker restart during call | persisted attempt is reconciled before another delivery |
| P14 | Vendor retention/expiry | expired provider resources remain explicit, not false success |

The complete local equivalent is:

```bash
./scripts/provider-local-gate.sh
```

The gate creates an isolated local D1 persistence directory, starts the
deterministic `scripts/provider-mock.py` boundary, starts a local Worker with
`external` mode, and runs the lifecycle smoke. To run the lower-level smoke
against an already configured Worker, set `BASE_URL` and use
`scripts/provider-lifecycle-smoke.sh` directly.

Run the local gate first, then repeat the lifecycle matrix against the selected
vendor sandbox with fresh test identifiers. Do not place vendor tokens in shell
history; use a temporary Wrangler secret store or the approved CI secret
mechanism.

## Promotion sequence

1. Select one reminder provider and, if needed, one messaging provider.
2. Confirm that both vendors support stable idempotency and an authoritative
   status lookup. Confirm reminder cancellation semantics before enabling Undo.
3. Capture the redacted P01–P14 evidence and vendor rate/retention limits.
4. Configure a separate staging Worker with `ACTION_PROVIDER_MODE=external`
   and action flags enabled only for sandbox test accounts.
5. Run the staging contract and provider lifecycle suites; verify metrics,
   alerting, retry, unknown-state reconciliation, and secret redaction.
6. Obtain Security/Operations approval and record the rollback owner.
7. Enable production action flags gradually behind a feature flag. Keep the
   previous safe configuration ready for immediate rollback.

## Rollback

If any provider test fails or the vendor reports an ambiguous result:

1. set the affected `ACTION_*_ENABLED` flag to `false`;
2. leave commands in `unknown`/retryable state for reconciliation;
3. do not delete provider-attempt or outbox records;
4. revoke or rotate the affected Wrangler secret if exposure is suspected;
5. reconcile already-created provider resources using the status endpoint;
6. record the incident and do not promote until the failed matrix rows pass.

No production deployment, migration, secret rotation, or vendor selection is
performed by this runbook.
