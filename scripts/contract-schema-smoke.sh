#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT="${ROOT_DIR}/contracts/openapi.yaml"

test -s "${CONTRACT}"
grep -q '^openapi: 3.1.0$' "${CONTRACT}"
grep -q '^  /health:$' "${CONTRACT}"
grep -q '^  /metrics:$' "${CONTRACT}"
grep -q '^  /v1/phone/commands:$' "${CONTRACT}"
grep -q '^  /v1/pairing/code/{code}:$' "${CONTRACT}"
grep -q '^  /v1/phone/pushes/{push_id}/dismiss:$' "${CONTRACT}"
grep -q '^  /v1/phone/sync:$' "${CONTRACT}"
grep -q '^  /v1/phone/events:$' "${CONTRACT}"
grep -q '^  /v1/phone/models/{model_id}:$' "${CONTRACT}"
grep -q '^  /v1/phone/retrievals/{retrieval_id}/download:$' "${CONTRACT}"
grep -q '^  /v1/agents/{agent_id}/rotate-key:$' "${CONTRACT}"
grep -q '^  /v1/skills:$' "${CONTRACT}"
grep -q '^  /v1/sessions/{session_id}:$' "${CONTRACT}"
grep -q '^  /v1/sessions/{session_id}/progress:$' "${CONTRACT}"
grep -q '^    CommandEnvelope:$' "${CONTRACT}"
grep -q '^    ModelManifest:$' "${CONTRACT}"
grep -q '^    ErrorResponse:$' "${CONTRACT}"
grep -q '^    CommandPage:$' "${CONTRACT}"
grep -q '^    ActionDescriptor:$' "${CONTRACT}"
grep -q '^    RetrievalItem:$' "${CONTRACT}"
grep -q '^    SkillDefinition:$' "${CONTRACT}"
grep -q '^    ProgressRequest:$' "${CONTRACT}"
grep -q 'download_path: { type: string }' "${CONTRACT}"
grep -q '^        - schema_version$' "${CONTRACT}"
grep -q '^        - idempotency_key$' "${CONTRACT}"
grep -q '^          type: string$' "${CONTRACT}"

CONTRACT="$CONTRACT" ruby <<'RUBY'
require "yaml"

contract = YAML.load_file(ENV.fetch("CONTRACT"))
abort "OpenAPI paths must be a mapping" unless contract.fetch("paths").is_a?(Hash)
abort "OpenAPI components are missing" unless contract.dig("components", "schemas").is_a?(Hash)
%w[/health /metrics /v1/phone/commands /v1/phone/sync /v1/phone/events].each do |path|
  abort "missing OpenAPI path: #{path}" unless contract.fetch("paths").key?(path)
end
abort "OpenAPI version must remain 3.1.0" unless contract.fetch("openapi") == "3.1.0"
puts "OpenAPI parser smoke passed: paths, schemas, and version are structurally valid"
RUBY

echo "contract schema smoke passed: OpenAPI 3.1 and required Knock Knock contracts are present"
