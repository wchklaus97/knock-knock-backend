#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HEALTHCHECK="${ROOT_DIR}/scripts/production-healthcheck.sh"

SMOKE_DIR="$(mktemp -d)"
trap 'rm -rf "$SMOKE_DIR"' EXIT
mkdir -p "$SMOKE_DIR/bin" "$SMOKE_DIR/state"

cat > "$SMOKE_DIR/bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

url="${*: -1}"
state_dir="${FAKE_CURL_STATE_DIR:?}"
mode="${FAKE_CURL_MODE:?}"
expected_version="${FAKE_CURL_EXPECTED_VERSION:?}"

if [[ "$url" == */health\?* ]]; then
  counter_file="$state_dir/health-count"
  count=0
  if [[ -f "$counter_file" ]]; then
    read -r count < "$counter_file"
  fi
  count=$((count + 1))
  printf '%s\n' "$count" > "$counter_file"

  version="$expected_version"
  if [[ "$mode" == "transient" && "$count" -lt 3 ]] || [[ "$mode" == "stale" ]]; then
    version="old-version"
  fi
  printf '{"ok":true,"api":"rust","runtime":"cloudflare-worker","version":"%s","push_mode":"both","apns_ready":true,"apns_production":true,"action_provider_mode":"external","action_provider_ready":false}\n' "$version"
  exit 0
fi

if [[ "$url" == */metrics\?* ]]; then
  cat <<'METRICS'
knock_knock_api_info{runtime="cloudflare-worker",api="rust"} 1
knock_knock_provider_ready 0
knock_knock_apns_ready 1
knock_knock_model_enabled 1
METRICS
  exit 0
fi

echo "unexpected fake curl URL: $url" >&2
exit 22
EOF
chmod +x "$SMOKE_DIR/bin/curl"

expected_version="expected-release-sha"
success_output="$SMOKE_DIR/success.out"
env \
  PATH="$SMOKE_DIR/bin:$PATH" \
  FAKE_CURL_STATE_DIR="$SMOKE_DIR/state" \
  FAKE_CURL_MODE=transient \
  FAKE_CURL_EXPECTED_VERSION="$expected_version" \
  KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION="$expected_version" \
  KNOCK_KNOCK_REQUIRE_RELEASE_READINESS_GAUGES=1 \
  KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_ATTEMPTS=3 \
  KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_DELAY_SECONDS=0 \
  "$HEALTHCHECK" https://production-healthcheck.invalid > "$success_output"
grep -Fq 'production healthcheck passed' "$success_output"
grep -Fqx '3' "$SMOKE_DIR/state/health-count"

rm -f "$SMOKE_DIR/state/health-count"
failure_output="$SMOKE_DIR/failure.out"
failure_error="$SMOKE_DIR/failure.err"
set +e
env \
  PATH="$SMOKE_DIR/bin:$PATH" \
  FAKE_CURL_STATE_DIR="$SMOKE_DIR/state" \
  FAKE_CURL_MODE=stale \
  FAKE_CURL_EXPECTED_VERSION="$expected_version" \
  KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION="$expected_version" \
  KNOCK_KNOCK_REQUIRE_RELEASE_READINESS_GAUGES=1 \
  KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_ATTEMPTS=2 \
  KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_DELAY_SECONDS=0 \
  "$HEALTHCHECK" https://production-healthcheck.invalid \
    > "$failure_output" 2> "$failure_error"
failure_status=$?
set -e
test "$failure_status" -eq 1
grep -Fq 'production health did not converge after 2 attempts' "$failure_error"
grep -Fq '"version":"old-version"' "$failure_error"
grep -Fqx '2' "$SMOKE_DIR/state/health-count"

invalid_error="$SMOKE_DIR/invalid.err"
set +e
env \
  PATH="$SMOKE_DIR/bin:$PATH" \
  KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_ATTEMPTS=0 \
  "$HEALTHCHECK" https://production-healthcheck.invalid \
    > /dev/null 2> "$invalid_error"
invalid_status=$?
set -e
test "$invalid_status" -eq 64
grep -Fq 'invalid production healthcheck attempt count' "$invalid_error"

echo "production healthcheck smoke passed: transient convergence, terminal diagnostics, and retry validation"
