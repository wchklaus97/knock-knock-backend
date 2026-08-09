#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

echo "[phase45] Rust format, tests, Clippy, and Worker target"
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo check --target wasm32-unknown-unknown

echo "[phase45] contract, migration, configuration, and adversarial checks"
./scripts/contract-schema-smoke.sh
./scripts/architecture-migration-smoke.sh
./scripts/adversarial-data-smoke.sh
./scripts/production-config-smoke.sh

echo "[phase45] repository hygiene"
git diff --check
if git ls-files | rg -n '(^|/)(\.env($|\.)|.*\.p8$|.*\.pem$|.*\.key$)' ; then
  echo "tracked secret-like file detected" >&2
  exit 1
fi
if git grep -nE 'BEGIN (OPENSSH|RSA|EC|PRIVATE) KEY|SUPABASE_SERVICE_ROLE_KEY[[:space:]]*=' -- ':!wrangler*.toml.example' ; then
  echo "private key or service-role secret detected in tracked source" >&2
  exit 1
fi

cat <<'EOF'
phase45 static release gate passed.
Required before production: deployed D1/E2E contract smoke, configured model
manifest/public-key rollout, security review, physical iPhone performance and
voice golden-set evidence, human approval of migration/APNs/model rollout.
EOF
