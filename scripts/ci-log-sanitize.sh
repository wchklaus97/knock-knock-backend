#!/usr/bin/env bash
set -euo pipefail

sanitize_stream() {
  sed -E \
    -e 's/([Aa]uthorization:[[:space:]]*[Bb]earer[[:space:]]+)[A-Za-z0-9._~+\/-]+/\1[REDACTED]/g' \
    -e 's/([Bb]earer[[:space:]]+)[A-Za-z0-9._~+\/-]+/\1[REDACTED]/g' \
    -e 's/(([Xx]-[Aa]gent-[Kk]ey|[Aa]pi-[Kk]ey):[[:space:]]*)[^[:space:]]+/\1[REDACTED]/g' \
    -e 's/((JWT_SECRET|ACTION_REMINDER_TOKEN|ACTION_MESSAGE_TOKEN|APNS_KEY|APNS_KEY_ID|APNS_TEAM_ID|SUPABASE_PUBLISHABLE_KEY|CLOUDFLARE_API_TOKEN|BACKUP_PASSPHRASE|SMOKE_PASSWORD)[[:space:]]*=[[:space:]]*)[^[:space:]]+/\1[REDACTED]/g' \
    -e 's/("[^"]*(token|password|api_key|secret|private_key)[^"]*"[[:space:]]*:[[:space:]]*")[^"]*"/\1[REDACTED]"/g' \
    -e 's/((token|password|api_key|secret|private_key)[[:space:]]*[=:][[:space:]]*)[^,[:space:]}]+/\1[REDACTED]/g' \
    -e 's/([?&](token|password|api_key|secret|key)=)[^&[:space:]]+/\1[REDACTED]/g'
}

if (($# == 0)); then
  sanitize_stream
else
  for log_file in "$@"; do
    test -f "${log_file}"
    sanitize_stream <"${log_file}"
  done
fi
