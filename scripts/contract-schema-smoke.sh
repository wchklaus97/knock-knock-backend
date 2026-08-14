#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT="${ROOT_DIR}/contracts/openapi.yaml"

test -s "${CONTRACT}"
grep -q '^openapi: 3.1.0$' "${CONTRACT}"
grep -q '^  /health:$' "${CONTRACT}"
grep -q '^  /metrics:$' "${CONTRACT}"
grep -q '^  /v1/phone/commands:$' "${CONTRACT}"
grep -q '^  /v1/phone/memories:$' "${CONTRACT}"
grep -q '^  /v1/phone/memories/{memory_id}:$' "${CONTRACT}"
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
grep -q '^    CreateMemoryRequest:$' "${CONTRACT}"
grep -q '^    MemoryItem:$' "${CONTRACT}"
grep -q '^    MemoryPage:$' "${CONTRACT}"
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
%w[/health /metrics /v1/phone/commands /v1/phone/memories /v1/phone/memories/{memory_id} /v1/phone/sync /v1/phone/events].each do |path|
  abort "missing OpenAPI path: #{path}" unless contract.fetch("paths").key?(path)
end
abort "OpenAPI version must remain 3.1.0" unless contract.fetch("openapi") == "3.1.0"

schemas = contract.dig("components", "schemas")
create = schemas.fetch("CreateMemoryRequest")
item = schemas.fetch("MemoryItem")
change = schemas.fetch("Change")

abort "memory create must reject unknown fields" unless create["additionalProperties"] == false
abort "memory item must reject projection drift" unless item["additionalProperties"] == false
abort "public memory writes must be v1" unless create.dig("properties", "schema_version", "const") == 1
abort "public memory writes must be explicit_user" unless create.dig("properties", "source_type", "const") == "explicit_user"
abort "public memory writes must be confirmed" unless create.dig("properties", "user_confirmed", "const") == true
abort "subject limit drift" unless create.dig("properties", "subject", "maxLength") == 100
abort "predicate limit drift" unless create.dig("properties", "predicate", "maxLength") == 100
abort "display_text limit drift" unless create.dig("properties", "display_text", "maxLength") == 2000
abort "locale bounds drift" unless create.dig("properties", "locale", "minLength") == 2 && create.dig("properties", "locale", "maxLength") == 35
abort "idempotency bounds drift" unless create.dig("properties", "idempotency_key", "minLength") == 8 && create.dig("properties", "idempotency_key", "maxLength") == 200
abort "value JSON byte limit drift" unless create.dig("properties", "value", "x-max-serialized-bytes") == 8192
abort "confidence bounds drift" unless create.dig("properties", "confidence", "minimum") == 0 && create.dig("properties", "confidence", "maximum") == 1
abort "retention must be strict timezone-bearing date-time" unless create.dig("properties", "retention_expires_at", "format") == "date-time" && create.dig("properties", "retention_expires_at", "pattern").include?("[Zz]")
abort "request hash must recursively canonicalize object keys" unless create.dig("x-request-hash", "canonicalization") == "recursively-sort-json-object-keys"

item_properties = item.fetch("properties")
abort "MemoryItem must use memory_id" unless item_properties.key?("memory_id")
%w[id user_id value_json request_hash idempotency_key deleted_at].each do |internal|
  abort "MemoryItem exposes internal field #{internal}" if item_properties.key?(internal)
end
abort "only display_text may enter E5" unless item.dig("x-e5-shadow-evaluator", "input-field") == "display_text"
abort "E5 embeddings must not persist" unless item.dig("x-e5-shadow-evaluator", "persists-embedding") == false
abort "E5 must not affect product behavior" unless item.dig("x-e5-shadow-evaluator", "affects") == []
abort "production ranking must require a new RFC" unless item.dig("x-e5-shadow-evaluator", "production-ranking-requires-new-rfc") == true
abort "phone changes must include memory" unless change.dig("properties", "entity_type", "enum").include?("memory")
puts "OpenAPI parser smoke passed: paths, schemas, and version are structurally valid"
RUBY

echo "contract schema smoke passed: OpenAPI 3.1 and required Knock Knock contracts are present"
