#!/usr/bin/env bash
set -euo pipefail

API="${1:-https://knock-knock-backend-production.wch-klaus.workers.dev}"
API="${API%/}"
PROBE="$(date +%s)"

health="$(curl --fail-with-body --silent --show-error "$API/health?probe=$PROBE")"
jq -e '(.ok == true) and (.api == "rust") and (.runtime == "cloudflare-worker")' <<<"$health" >/dev/null

metrics="$(curl --fail-with-body --silent --show-error "$API/metrics?probe=$PROBE")"
grep -q 'knock_knock_api_info{runtime="cloudflare-worker",api="rust"} 1' <<<"$metrics"

jq -c '{ok,api,runtime,version}' <<<"$health"
echo "production healthcheck passed: $API"
