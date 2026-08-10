# Knock Knock Staging Verification

**Date:** 2026-08-10 (Asia/Hong_Kong)

**Scope:** Remote staging Worker/D1/R2 deployment for the Phase 4/5 completion
branch. This document records staging evidence only; it is not production
approval.

## Deployed environment

- Worker: `https://knock-knock-backend-staging.wch-klaus.workers.dev`
- Backend commit: `020d354` (`feat(backend): enforce registry command policy`)
- Supabase: dedicated `knock-knock-staging` project
- D1: dedicated `knock-knock-staging` database
- R2: dedicated private `knock-knock-staging` bucket
- `NODE_ENV`: `staging`
- `AUTH_PROVIDER`: `supabase`
- `PUSH_MODE`: `dev`
- `ACTION_PROVIDER_MODE`: `disabled`
- Reminder/message effects: disabled
- Voice model rollout: disabled

Staging intentionally uses the development push inbox and disabled external
effects. It is safe for contract, sync, history, retrieval, and iOS integration
testing. Real APNs is a separate sandbox/device gate and must not be inferred
from staging health.

## Evidence completed

- Worker deployment succeeded with the current backend bundle.
- `/health` returned `ok=true`, Rust Worker runtime, staging release version,
  `push_mode=dev`, disabled action provider, and no external action readiness.
- `/metrics` returned the API, provider-readiness, and APNs-readiness gauges.
- Remote D1 schema was queried after migration verification; all foundation,
  command, history, retrieval, outbox, rate-limit, and vertical-action tables
  were present.
- Direct remote R2 put/get and byte-for-byte fixture comparison passed.
- GitHub repository `staging` environment was created with the non-secret
  deployment variables required by `.github/workflows/staging-contract-gate.yml`.
- iOS `Staging` configuration was added and built successfully for the iOS
  Simulator. It uses the staging HTTPS Worker URL and the development APNs
  entitlement.
- After reclaiming simulator disk space and erasing the affected simulator,
  the iOS regression suite passed with 37 unit tests and 3 UI tests. That
  regression used the local Worker fixture, so it proves the app shell and
  test harness are healthy but does not replace the remote staging auth or
  physical-device gates below.

## Full gate status

`scripts/staging-contract-gate.sh` is prepared but was not fully completed.
The first Supabase UAT account creation attempt was rejected by the dedicated
project's email provider rate limit:

```text
over_email_send_rate_limit
```

Therefore login, refresh/logout, user isolation, phone contract, and remote R2
route retention tests are **not declared passed**. No password or token was
written to the repository or GitHub.

The GitHub environment still needs these secrets before its manual workflow can
run:

- `CLOUDFLARE_API_TOKEN` — a least-privilege non-interactive Cloudflare token
- `KNOCK_KNOCK_STAGING_SMOKE_EMAIL` — a staging Supabase UAT account
- `KNOCK_KNOCK_STAGING_SMOKE_PASSWORD` — its password

The local Wrangler OAuth session was used only for the manual deployment and
was not copied into GitHub Actions.

## Next controlled steps

1. Create or provide one staging Supabase UAT account after the email provider
   rate limit clears; do not reuse a production password.
2. Add the three GitHub staging secrets above.
3. Run the `Staging contract gate` workflow with the Worker URL.
4. Build/install the iOS `Staging` configuration on a physical iPhone and
   verify login, pairing, inbox refresh, SSE recovery, offline queue recovery,
   and a second device's cursor convergence.
5. Run the separate APNs sandbox gate with a real device token. Simulator
   notification banners do not count as APNs evidence.

## Rollback

No production resource was changed. Roll back staging by routing the Worker to
the previous Cloudflare Worker version; do not delete the staging D1/R2
resources. The applied migrations are additive and must be rolled back only
through an approved migration plan.
