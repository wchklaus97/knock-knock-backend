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
- `PUSH_MODE`: `both` (APNs sandbox plus development push inbox)
- `APNS_PRODUCTION`: `false`
- `ACTION_PROVIDER_MODE`: `disabled`
- Reminder/message effects: disabled
- Voice model rollout: disabled

Staging intentionally uses the APNs sandbox alongside the development push
inbox and keeps external action effects disabled. `apns_ready=true` means that
the signing configuration is present; it is not evidence of APNs delivery or
physical-device completion. `APNS_PRODUCTION=false` is mandatory. Real APNs
device delivery remains a separate sandbox/device gate, and production APNs
rollout remains deferred.

## Evidence completed

- Worker deployment succeeded with the current backend bundle.
- The current staging `/health` profile is `push_mode=both`,
  `apns_ready=true`, and `apns_production=false`, with the action provider
  disabled and no external action readiness.
- `/metrics` returned the API, provider-readiness, and APNs-readiness gauges.
- Remote D1 schema was queried after migration verification; all foundation,
  command, history, retrieval, outbox, rate-limit, and vertical-action tables
  were present.
- Direct remote R2 put/get and byte-for-byte fixture comparison passed.
- GitHub repository `staging` environment was created with the non-secret
  deployment variables required by `.github/workflows/staging-contract-gate.yml`.
- A staging-only UAT user was provisioned in the dedicated Supabase project
  with an email-confirmed account; login, protected API access, refresh, and
  logout passed against the staging Worker. Its email and password are stored
  only as GitHub environment secrets.
- Staging Supabase Email auth was enabled and `mailer_autoconfirm` was set to
  `true`; registration smoke no longer depends on the built-in email provider.
- The remote R2 smoke now waits for the deployed one-minute Cloudflare cron
  instead of calling the local-only `/__scheduled` test path.
- iOS `Staging` configuration was added and built successfully for the iOS
  Simulator. It uses the staging HTTPS Worker URL and the development APNs
  entitlement.
- After reclaiming simulator disk space and erasing the affected simulator,
  the iOS regression suite passed with 37 unit tests and 3 UI tests. That
  regression used the local Worker fixture, so it proves the app shell and
  test harness are healthy but does not replace the remote staging auth or
  physical-device gates below.

## Full gate status

`scripts/staging-contract-gate.sh` passed against the deployed staging Worker
after the staging-only Email auth configuration was corrected:

- Supabase auth: login, protected API, refresh, and logout
- Rust contract: phone routes, command pagination, pairing isolation/expiry,
  push isolation/dismissal, action descriptors, confirmation, claim, result,
  and refresh
- Remote R2: stream, metadata, user namespace, cross-user isolation, shared-key
  retention, and object deletion

The gate was run locally with the authenticated Wrangler OAuth session for the
remote D1/R2 smoke. It is not yet a GitHub Actions result because the workflow
requires a non-interactive Cloudflare API token.

The GitHub environment now contains the staging UAT email/password secrets.
It still needs one secret before the manual workflow can run:

- `CLOUDFLARE_API_TOKEN` — a least-privilege non-interactive Cloudflare token

The local Wrangler OAuth session was used only for the manual deployment and
was not copied into GitHub Actions.

## Next controlled steps

1. Add a least-privilege `CLOUDFLARE_API_TOKEN` to the GitHub `staging`
   environment; do not reuse the local Wrangler OAuth session.
2. Run the `Staging contract gate` workflow with the Worker URL.
3. Build/install the iOS `Staging` configuration on a physical iPhone and
   verify login, pairing, inbox refresh, SSE recovery, offline queue recovery,
   and a second device's cursor convergence.
4. Run the separate APNs sandbox gate with a real device token. Simulator
   notification banners do not count as APNs evidence.

## Rollback

No production resource was changed. Roll back staging by routing the Worker to
the previous Cloudflare Worker version; do not delete the staging D1/R2
resources. The applied migrations are additive and must be rolled back only
through an approved migration plan.

## Manual staging deployment workflow (prepared, not executed)

`.github/workflows/staging-deploy.yml` is a `workflow_dispatch`-only release
entry point. The workflow itself has not been executed from this branch: this
PR ran no workflow dispatch, Worker deployment, remote migration, or staging
contract gate, and changed no Cloudflare state.

Before using it, keep the existing `staging` GitHub environment protected with
required reviewers and provision only its staging-scoped values: the
Cloudflare account ID, staging D1 database ID, private staging R2 bucket,
staging Supabase URL, staging CORS origin, staging release version, and a
least-privilege `CLOUDFLARE_API_TOKEN`. The workflow materializes the checked-in
staging template in the runner temporary directory without printing its values;
it does not read `.dev.vars`, Wrangler OAuth state, `wrangler.production.toml`,
or production secrets.

Runbook:

1. Dispatch with the HTTPS staging Worker URL and leave `apply_migrations` at
   its default `false`. This deploys only the staging Worker/D1/R2 bindings
   and never invokes a migration or production command.
2. Confirm the workflow's post-deploy `/health` assertion: Rust Worker,
   `push_mode=both`, `apns_ready=true`, `apns_production=false`, and the action
   provider disabled. This is a deployment smoke, not the full contract gate.
3. Run the existing **Staging contract gate** workflow separately against the
   same URL; it remains the owner of authenticated REST/SSE, D1, R2, and
   isolation evidence.
4. Only when the expand review is approved, dispatch the workflow with
   `apply_migrations=true`. The separate protected staging job copies exactly
   `migrations/0013_retrieval_retention_status.sql` and
   `migrations/0014_command_safety.sql` into a temporary migration directory,
   then runs Wrangler's remote migration command with that directory. A
   staging D1 already containing the earlier schema is required; no other
   migration file is made available to the command.

Rollback/runbook notes: if the Worker deploy is unhealthy, route staging back
to the previous Cloudflare Worker version and keep the staging D1/R2 resources
in place. If the explicit migration path has run, preserve the Wrangler/D1
backup and use the approved forward/compensating migration plan; do not run
destructive rollback SQL and do not point this workflow at production.
