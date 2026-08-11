# Knock Knock Staging Verification

**Last verified:** 2026-08-11 (Asia/Hong_Kong)

**Scope:** Remote staging Worker/D1/R2 deployment for the Phase 4/5 completion
branch. This document records staging evidence only; it is not production
approval.

## Deployed environment

- Worker: `https://knock-knock-backend-staging.wch-klaus.workers.dev`
- Backend commit: `c83b04d` (merged PR #27)
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

- The protected GitHub **Staging deploy** workflow succeeded for `c83b04d`
  without applying migrations: [run 31496029273](https://github.com/wchklaus97/knock-knock-backend/actions/runs/31496029273).
- The protected GitHub **Staging contract gate** succeeded for the same commit:
  [run 31496803603](https://github.com/wchklaus97/knock-knock-backend/actions/runs/31496803603).
- Twenty consecutive read-only `/health` probes passed on 2026-08-11 with
  Rust Worker, APNs sandbox signing ready, production APNs disabled, and the
  external action provider disabled.
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

The staging GitHub environment now has the scoped non-interactive Cloudflare
credential and UAT credentials needed by these protected workflows. Secret
values were not printed or copied into the repository.

## Next controlled steps

1. Review and merge the paired voice-workflow PRs. The changes documented in
   `VOICE_MODEL_RELEASE_RUNBOOK.md` are not deployed by the evidence above.
2. Build/install that exact iOS `Staging` revision on the connected iPhone 13
   and verify real microphone/VAD interruptions, cold-launch command recovery,
   memory, thermal state, and crash count.
3. When both phones are available, verify same-account cursor, tombstone, and
   command convergence on two physical devices.
4. Run the separate APNs sandbox delivery gate with a real device token. Simulator
   notification banners do not count as APNs evidence.
5. Keep `ACTION_PROVIDER_MODE=disabled` until a selected provider passes its
   sandbox contract; `action_provider_ready=false` is expected in staging.

## Rollback

No production resource was changed. Roll back staging by routing the Worker to
the previous Cloudflare Worker version; do not delete the staging D1/R2
resources. The applied migrations are additive and must be rolled back only
through an approved migration plan.

## Protected staging deployment workflow

`.github/workflows/staging-deploy.yml` is a `workflow_dispatch`-only release
entry point. It was executed successfully for `c83b04d` with
`apply_migrations=false`; the separate migration job was skipped. This local
voice completion worktree has not dispatched a workflow, deployed a Worker,
changed a secret, or applied a remote migration.

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
