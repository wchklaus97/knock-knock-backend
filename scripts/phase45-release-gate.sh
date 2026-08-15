#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"
./scripts/ci-prerequisites.sh static

echo "[phase45] Rust format, tests, Clippy, and Worker target"
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo check --target wasm32-unknown-unknown

echo "[phase45] contract, migration, configuration, and adversarial checks"
./scripts/contract-schema-smoke.sh
./scripts/contract-breaking-smoke.sh
./scripts/contract-route-parity-smoke.sh
./scripts/provider-safety-smoke.sh
./scripts/architecture-migration-smoke.sh
./scripts/adversarial-data-smoke.sh
./scripts/execution-time-authority-smoke.sh
./scripts/production-config-smoke.sh
./scripts/production-healthcheck-smoke.sh
./scripts/backup-restore-smoke.sh
./scripts/ci-log-sanitization-smoke.sh
./scripts/voice-model-candidate-smoke.sh
./scripts/voice-model-release-smoke.sh
test -x ./scripts/local-contract-gate.sh
test -x ./scripts/memory-contract-smoke.sh
test -x ./scripts/execution-time-authority-smoke.sh
test -x ./scripts/r2-download-smoke.sh
test -x ./scripts/provider-lifecycle-smoke.sh
test -x ./scripts/provider-local-gate.sh
test -x ./scripts/provider-observability-smoke.sh
test -x ./scripts/provider-mock.py
test -x ./scripts/rate-limit-smoke.sh
test -x ./scripts/production-healthcheck.sh
test -x ./scripts/production-healthcheck-smoke.sh
test -x ./scripts/staging-contract-gate.sh
test -x ./scripts/voice-model-candidate.sh
test -x ./scripts/voice-model-candidate-smoke.sh
test -x ./scripts/voice-model-release.sh
test -x ./scripts/voice-model-release-smoke.sh
test -x ./scripts/voice-model-r2-smoke.sh
bash -n \
  ./scripts/ci-log-sanitize.sh \
  ./scripts/ci-log-sanitization-smoke.sh \
  ./scripts/execution-time-authority-smoke.sh \
  ./scripts/local-contract-gate.sh \
  ./scripts/memory-contract-smoke.sh \
  ./scripts/r2-download-smoke.sh \
  ./scripts/provider-lifecycle-smoke.sh \
  ./scripts/provider-local-gate.sh \
  ./scripts/provider-observability-smoke.sh \
  ./scripts/rate-limit-smoke.sh \
  ./scripts/production-healthcheck.sh \
  ./scripts/production-healthcheck-smoke.sh \
  ./scripts/staging-contract-gate.sh \
  ./scripts/voice-model-candidate.sh \
  ./scripts/voice-model-candidate-smoke.sh \
  ./scripts/voice-model-release.sh \
  ./scripts/voice-model-release-smoke.sh \
  ./scripts/voice-model-r2-smoke.sh
python3 -c 'import ast, pathlib; ast.parse(pathlib.Path("scripts/provider-mock.py").read_text())'

echo "[phase45] repository hygiene"
git diff --check
secret_like_files="$(git ls-files | grep -En '(^|/)(\.env($|\.)|.*\.p8$|.*\.pem$|.*\.key$)' || true)"
if [[ -n "${secret_like_files}" ]]; then
  printf '%s\n' "${secret_like_files}" >&2
  echo "tracked secret-like file detected" >&2
  exit 1
fi
if git grep -qE 'BEGIN (OPENSSH|RSA|EC|PRIVATE) KEY|SUPABASE_SERVICE_ROLE_KEY[[:space:]]*=' -- ':!wrangler*.toml.example'; then
  echo "private key or service-role secret detected in tracked source" >&2
  exit 1
fi
if git grep -qE '(^|[[:space:]])[r][g]([[:space:]]|$)' -- .github/workflows scripts; then
  echo "CI gate references an undeclared ripgrep command; use portable grep or install it in CI" >&2
  exit 1
fi

cat <<'EOF'
phase45 static release preflight passed.
This output is limited to the local static checks above; it does not claim
staging deployment, APNs delivery, physical-iPhone performance, voice
golden-set, provider sandbox, or production rollout evidence. Those remain
explicit release prerequisites.
EOF
