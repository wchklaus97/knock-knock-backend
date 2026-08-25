#!/usr/bin/env bash
# Headless Ask listening-window gate: no phone, no deploy, no send enablement.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cargo test --lib asks::tests -- --nocapture
