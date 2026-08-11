#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SANITIZER="${ROOT_DIR}/scripts/ci-log-sanitize.sh"

test -x "${SANITIZER}"

fixture="ci-log-fixture-value"
raw_log="$(printf '%s\n' \
  "Authorization: Bearer ${fixture}" \
  "X-Agent-Key: ${fixture}" \
  "JWT_SECRET=${fixture}" \
  "payload={\"access_token\":\"${fixture}\",\"password\":\"${fixture}\"}")"
redacted="$(printf '%s\n' "${raw_log}" | "${SANITIZER}")"

if grep -Fq "${fixture}" <<<"${redacted}"; then
  echo "ci log sanitization smoke failed: fixture credential survived redaction" >&2
  exit 1
fi
grep -Fq '[REDACTED]' <<<"${redacted}"

echo "ci log sanitization smoke passed: bearer, header, assignment, and JSON credential shapes are redacted"
