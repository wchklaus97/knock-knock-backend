#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT="${ROOT_DIR}/contracts/openapi.yaml"

test -s "${CONTRACT}"
grep -q '^openapi: 3.1.0$' "${CONTRACT}"
grep -q '^  /v1/phone/commands:$' "${CONTRACT}"
grep -q '^  /v1/phone/sync:$' "${CONTRACT}"
grep -q '^  /v1/phone/events:$' "${CONTRACT}"
grep -q '^    CommandEnvelope:$' "${CONTRACT}"
grep -q '^    ErrorResponse:$' "${CONTRACT}"
grep -q '^        - schema_version$' "${CONTRACT}"
grep -q '^        - idempotency_key$' "${CONTRACT}"
grep -q '^          type: string$' "${CONTRACT}"

echo "contract schema smoke passed: OpenAPI 3.1 and required Knock Knock contracts are present"
