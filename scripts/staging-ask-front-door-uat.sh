#!/usr/bin/env bash
# Staging UAT gate for the Ask front door: sending stays off. No Production deploy.
set -euo pipefail

BASE_URL="${BASE_URL:-https://knock-knock-backend-staging.wch-klaus.workers.dev}"
BASE_URL="${BASE_URL%/}"

case "${BASE_URL}" in
  https://*) ;;
  *)
    echo "staging Ask UAT requires an HTTPS staging URL" >&2
    exit 64
    ;;
esac
if [[ "${BASE_URL}" == *production* ]]; then
  echo "staging Ask UAT refuses a production-looking URL" >&2
  exit 64
fi

health="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "${BASE_URL}/health")"
echo "$health" | jq '{ok,api,runtime,version,action_provider_mode,action_provider_ready,action_message_enabled}'
jq -e '
  (.ok == true) and
  (.api == "rust") and
  (.runtime == "cloudflare-worker") and
  (.action_provider_ready == false) and
  (
    (.action_message_enabled == false) or
    (.action_message_enabled == null)
  )
' <<<"$health" >/dev/null

echo "staging Ask UAT: sending stays off"
