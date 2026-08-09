# knock-knock-backend

Rust API for Knock Knock, deployed as a Cloudflare Worker with D1 SQLite storage.

The route contract intentionally stays compatible with the existing iPhone and
MCP clients:

- `/v1/auth/*` — user registration, login, refresh and logout
- `/v1/agents/*` and `/v1/pairing/*` — agent keys and one-time pairing
- `/v1/skills` — skill definitions
- `/v1/sessions/*` and `/v1/actions/*` — agent session/action loop
- `/v1/phone/*` — phone inbox, replies, confirmations and devices
- `/v1/dev/pushes` — development push inbox

## Local development

Requirements: Rust, the `wasm32-unknown-unknown` target, `worker-build`, and
Wrangler.

```sh
cargo check --target wasm32-unknown-unknown
worker-build --release
wrangler d1 migrations apply DB --local
wrangler dev
```

Run the repeatable contract loop while `wrangler dev` is running. It covers
the full auth/pairing/phone loop and two consecutive decisions on the same
`session_id` and `chat_id`:

```sh
./scripts/contract-smoke.sh
```

The local endpoint is `http://127.0.0.1:8787`. For the iPhone, use the Mac's
LAN address instead of `127.0.0.1`.

Set a development secret in `.dev.vars`:

```text
JWT_SECRET=replace-this-for-local-development
```

The checked-in `wrangler.toml` is local-only: it explicitly sets
`NODE_ENV=development` and binds to local D1. Do not use it for a remote
production deployment. Prepare the production config first:

```sh
cp wrangler.production.toml.example wrangler.production.toml
wrangler d1 create knock-knock
# Put the returned UUID into wrangler.production.toml as database_id and
# replace CORS_ORIGIN and SERVICE_VERSION.
wrangler d1 migrations apply knock-knock --remote --config wrangler.production.toml
wrangler secret put JWT_SECRET --config wrangler.production.toml
# Required for real APNs delivery (use the .p8 contents as the key value).
wrangler secret put APNS_KEY --config wrangler.production.toml
wrangler secret put APNS_KEY_ID --config wrangler.production.toml
wrangler secret put APNS_TEAM_ID --config wrangler.production.toml
wrangler deploy --config wrangler.production.toml
```

The Worker rejects production traffic until `NODE_ENV=production`, a random
`JWT_SECRET` (at least 32 characters), explicit CORS, APNs credentials,
`PUSH_MODE=apns|both`, `APNS_PRODUCTION`, and `SERVICE_VERSION` are present.
It returns a generic message for HTTP 5xx responses so database or signing
details are not leaked to clients.

`PUSH_MODE=dev` stores push events in D1 so the current iPhone development
inbox continues to work. For a production phone build, set `PUSH_MODE=apns`
or `PUSH_MODE=both`, configure `APNS_BUNDLE_ID` and
`APNS_PRODUCTION=true`, and register the iOS device token through
`POST /v1/phone/devices`. The Worker signs APNs provider tokens with the
configured Apple `.p8` key; missing or failed APNs delivery falls back to the
development inbox when appropriate instead of being reported as a false
success.

Local action execution uses `ACTION_PROVIDER_MODE=internal` and may enable the
reminder/message flags for D1-only testing. The Worker also supports a reviewed
HTTPS webhook adapter in `ACTION_PROVIDER_MODE=external`: set delivery,
status, and (for reminders) cancellation endpoints, plus the matching
secret-only `ACTION_REMINDER_TOKEN`/`ACTION_MESSAGE_TOKEN`. Every request
carries the command idempotency key. A provider timeout becomes
`unknown/retryable`; the scheduled worker queries the configured status
endpoint before materializing a success, and local Undo calls the reviewed
reminder cancellation endpoint before changing D1 state.
Production keeps both action flags disabled until the provider delivery/status
endpoints, idempotency behavior, cancellation policy, and credentials are approved. A
local queued message is never reported as externally delivered.

For a safe remote staging Worker, copy `wrangler.staging.toml.example`, create
a separate D1 database and Supabase project, replace its explicit origin and
release version, then apply migrations with that config. Staging intentionally
uses `PUSH_MODE=dev` and `ACTION_PROVIDER_MODE=disabled`; it verifies auth,
contract, sync, history, and UI behavior without sending real reminders or
messages. It is not a production substitute.

Retrieval payloads use the `R2` binding. The API returns a user-scoped
`download_path` and streams the object through
`GET /v1/phone/retrievals/{retrieval_id}/download`; it never returns the
internal `r2_key`, uses `private, no-store`, and rejects expired, deleted, or
cross-user retrievals.

`PUSH_MODE=dev` is not APNs: it writes a push event to the D1-backed development
inbox for polling. Production uses `PUSH_MODE=both` during rollout so the app
can keep the inbox fallback while Apple delivery is verified; use
`APNS_PRODUCTION=true` for TestFlight/App Store builds. An Xcode development
build uses the sandbox APNs environment and must use a separate config with
`APNS_PRODUCTION=false` if it is tested against APNs directly.

## Supabase Auth

Production authentication uses Supabase Auth while Knock Knock's agent,
session, device and push records remain in D1. The Worker keeps the same
`/v1/auth/*` contract for the iPhone: it forwards password registration/login
and refresh requests to Supabase, then maps the Supabase user ID to a local D1
user row. It never stores the user's password in D1.

The production config sets `AUTH_PROVIDER=supabase` and `SUPABASE_URL`. Add the
project's publishable key as a Worker secret; do not use the `service_role` key:

```sh
wrangler secret put SUPABASE_PUBLISHABLE_KEY --config wrangler.production.toml
```

Apply the D1 migration before deploying the Supabase-backed Worker:

```sh
wrangler d1 migrations apply knock-knock --remote --config wrangler.production.toml
```

For the first workflow test, create one user in Supabase Authentication →
Users. The email/password screen in the iPhone continues to call the existing
bridge API, so no password or Supabase key is bundled in the app.

After deploying, run the repeatable auth UAT without printing tokens:

```sh
SMOKE_EMAIL='your-uat-email' \
SMOKE_PASSWORD='your-uat-password' \
./scripts/supabase-auth-smoke.sh
```

## Operations

Production Wrangler config enables Workers Observability. Run the repeatable
probe with:

```sh
./scripts/production-healthcheck.sh https://your-worker.workers.dev
```

D1 production databases provide point-in-time recovery through Cloudflare
Time Travel. For an export that must be retained outside D1, run:

```sh
./scripts/export-production-d1.sh /secure/backups/knock-knock-$(date +%Y%m%d).sql
```

The scheduled GitHub workflow runs the health probe every ten minutes. A
failure opens one `production-alert` issue and a later healthy run closes it;
enable GitHub Actions and issue notifications for the account/team that should
receive failures.

The scheduled D1 backup workflow exports the production database daily and
retains a GitHub Actions artifact for 30 days. Before enabling it, add the
repository Actions secret `CLOUDFLARE_API_TOKEN` (D1 read access); never put
that value in this repository. The non-secret repository Actions variables
`KNOCK_KNOCK_CLOUDFLARE_ACCOUNT_ID`, `KNOCK_KNOCK_D1_DATABASE_ID`, and
`KNOCK_KNOCK_CORS_ORIGIN` are non-secret deployment settings used to
materialize the ignored production Wrangler config during the job. The workflow
can also be started manually
from the Actions tab.

The old Node API remains in the original `voice-agent-bridge` tree as a
migration reference. The Rust Worker is now the backend in this repository and
uses the same HTTP paths and auth headers, so existing iPhone, Codex, Cursor,
and Paperclip clients can switch by pointing `BRIDGE_API_URL` at this Worker.
