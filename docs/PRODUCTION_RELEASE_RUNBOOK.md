# Production Release Runbook

## 中文摘要

生产发布必须由 GitHub `production` environment 的人工审批保护。每次发布都要先完成可恢复的加密 D1 备份，记录当前 Cloudflare Worker version ID，并指定唯一的 `main` commit SHA。任何脚本都不得自动批准、自动发布或自动回滚生产。

## Preconditions

Configure release/deployment values in the GitHub `production` environment,
never in repository files or workflow output:

- secret: `CLOUDFLARE_API_TOKEN`
- variable: `KNOCK_KNOCK_CLOUDFLARE_ACCOUNT_ID`
- variable: `KNOCK_KNOCK_D1_DATABASE_ID`
- variable: `KNOCK_KNOCK_R2_BUCKET`
- variable: `KNOCK_KNOCK_SUPABASE_URL`
- variable: `KNOCK_KNOCK_CORS_ORIGIN`
- provider URL variables used by `.github/workflows/production-release.yml`
- signed voice-model URL, R2 key, manifest JSON, and expiry variables used by
  `.github/workflows/production-release.yml`

Configure the scheduled backup job in a separate GitHub
`production-backup` environment:

- secret: `CLOUDFLARE_API_TOKEN` using a dedicated least-privilege token that
  can read the production D1 database and write/read only the backup R2 bucket
- secret: `KNOCK_KNOCK_BACKUP_PASSPHRASE`
- variable: `KNOCK_KNOCK_CLOUDFLARE_ACCOUNT_ID`
- variable: `KNOCK_KNOCK_D1_DATABASE_ID`
- variable: `KNOCK_KNOCK_BACKUP_BUCKET`
- variable: `KNOCK_KNOCK_CORS_ORIGIN`

Restrict `production-backup` to the repository's protected `main` branch. Do
not add a required reviewer to that environment: a required reviewer would
leave every scheduled backup waiting for manual approval. It must not contain
Worker-deploy, APNs, Supabase, action-provider, or voice-model secrets.

The Worker secrets `JWT_SECRET`, `SUPABASE_PUBLISHABLE_KEY`, `APNS_KEY`,
`APNS_KEY_ID`, `APNS_TEAM_ID`, and any provider tokens must already exist in
the production Worker secret store. The release workflow never prints or
recreates them.

Protect the `production` environment with at least one required reviewer and
disable administrator bypass. Protect `main` with required backend CI and a
pull-request review. Keep the independently scoped `production-backup`
environment automatic and branch-restricted as described above. These controls
are external repository settings and must be verified in the release record.

## Release

1. Confirm CI passed on the exact 40-character `main` SHA.
2. Run **Production D1 backup** and retain its successful workflow run ID.
   The job uploads an encrypted object, downloads it, compares ciphertext,
   decrypts it, and verifies a non-empty SQL schema before succeeding.
3. Record the current healthy Cloudflare Worker version ID. This is the
   rollback target, not the Git commit SHA.
4. Review every pending D1 migration. Migration `0014` and any destructive or
   data-rewriting migration require an explicit maintenance plan and separate
   approval. Never infer schema rollback from a Worker rollback.
5. Dispatch **Production release (human approved)** with the exact SHA,
   successful backup run ID, rollback version ID, migration choice, and exact
   approval phrase.
6. A required environment reviewer approves the job. The workflow rechecks
   that the SHA is current `origin/main`, runs local release gates, deploys the
   exact build, and verifies the service version and readiness gauges.
7. Retain the generated release record with the reviewed change ticket.

## Rollback

For a code/configuration regression, disable newly enabled provider/model
feature flags first when that safely stops side effects. Reconcile commands in
`unknown` or retryable states before changing execution behavior.

Dispatch **Production rollback (human approved)** with the failed SHA,
previously recorded healthy Cloudflare version ID, expected prior service
version, and exact approval phrase. A protected environment reviewer must
approve it. The job rolls back Worker code and verifies the expected service
version.

Worker rollback does not reverse D1 migrations. Restore or compensate data
only from an independently reviewed database recovery plan. Do not run
down-migrations automatically.

## Evidence and failure policy

- `apns_ready=true` proves credential/configuration readiness, not delivery.
- `action_provider_ready=true` proves adapter configuration, not vendor
  semantics or successful side effects.
- A local or staging test never substitutes for production approval.
- A failed backup, missing setting, placeholder value, version mismatch,
  health failure, or readiness failure blocks release.
- Do not include tokens, `.p8` material, user audio, full sensitive command
  text, or database exports in GitHub artifacts or logs.
