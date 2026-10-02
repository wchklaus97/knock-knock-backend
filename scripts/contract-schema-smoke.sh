#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT="${ROOT_DIR}/contracts/openapi.yaml"

test -s "${CONTRACT}"
grep -q '^openapi: 3.1.0$' "${CONTRACT}"
grep -q '^  /health:$' "${CONTRACT}"
grep -q '^  /ready:$' "${CONTRACT}"
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
grep -q '^  /v1/agents/me/asks/claim:$' "${CONTRACT}"
grep -q '^  /v1/agents/me/listener/heartbeat:$' "${CONTRACT}"
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
grep -q '^    ListenerDisconnectResponse:$' "${CONTRACT}"
grep -q 'download_path: { type: string }' "${CONTRACT}"
grep -q '^        - schema_version$' "${CONTRACT}"
grep -q '^        - idempotency_key$' "${CONTRACT}"
grep -q '^          type: string$' "${CONTRACT}"

CONTRACT="$CONTRACT" ruby <<'RUBY'
require "yaml"

contract = YAML.load_file(ENV.fetch("CONTRACT"))
abort "OpenAPI paths must be a mapping" unless contract.fetch("paths").is_a?(Hash)
abort "OpenAPI components are missing" unless contract.dig("components", "schemas").is_a?(Hash)
%w[/health /ready /metrics /v1/phone/commands /v1/phone/memories /v1/phone/memories/{memory_id} /v1/phone/sync /v1/phone/events /v1/agents/me/asks /v1/agents/me/asks/claim /v1/agents/me/listener /v1/agents/me/listener/heartbeat].each do |path|
  abort "missing OpenAPI path: #{path}" unless contract.fetch("paths").key?(path)
end
abort "OpenAPI version must remain 3.1.0" unless contract.fetch("openapi") == "3.1.0"

schemas = contract.dig("components", "schemas")
health = schemas.fetch("HealthResponse")
readiness = schemas.fetch("ReadinessResponse")
agent = schemas.fetch("Agent")
create = schemas.fetch("CreateMemoryRequest")
item = schemas.fetch("MemoryItem")
change = schemas.fetch("Change")

ask_get = contract.dig("paths", "/v1/agents/me/asks", "get")
ask_claim = contract.dig("paths", "/v1/agents/me/asks/claim", "post")
listener_post = contract.dig("paths", "/v1/agents/me/listener", "post")
listener_delete = contract.dig("paths", "/v1/agents/me/listener", "delete")
listener_heartbeat = contract.dig("paths", "/v1/agents/me/listener/heartbeat", "post")
progress_operation = contract.dig("paths", "/v1/sessions/{session_id}/progress", "post")
listener_headers = %w[ListenerChatId ListenerInstanceId ListenerLeaseId ListenerGeneration].map { |name| "#/components/parameters/#{name}" }

abort "GET Ask polling must not expose claim query behavior" if ask_get.fetch("parameters", []).any? { |parameter| parameter["$ref"] == "#/components/parameters/Claim" || parameter["name"] == "claim" }
abort "Ask claim must be a POST" unless ask_claim
[ask_get, ask_claim, listener_delete].each do |operation|
  refs = operation.fetch("parameters", []).map { |parameter| parameter["$ref"] }
  abort "lease-v2 headers are incomplete" unless (listener_headers - refs).empty?
end
abort "listener registration schema drift" unless listener_post.dig("requestBody", "content", "application/json", "schema", "$ref") == "#/components/schemas/ListenerRegistrationRequest"
abort "listener heartbeat schema drift" unless listener_heartbeat.dig("requestBody", "content", "application/json", "schema", "$ref") == "#/components/schemas/ListenerHeartbeatRequest"
abort "listener release must require agent authentication" unless listener_delete.fetch("security") == [{"agentKey" => []}]
abort "listener release idempotency marker drift" unless listener_delete["x-idempotent"] == true
abort "listener release must not accept a JSON body" if listener_delete.key?("requestBody")
abort "listener release response schema drift" unless listener_delete.dig("responses", "200", "content", "application/json", "schema", "$ref") == "#/components/schemas/ListenerDisconnectResponse"
progress_headers = %w[ProgressListenerChatId ProgressListenerInstanceId ProgressListenerLeaseId ProgressListenerGeneration].map { |name| "#/components/parameters/#{name}" }
progress_refs = progress_operation.fetch("parameters", []).map { |parameter| parameter["$ref"] }
abort "Ask progress listener headers are incomplete" unless (progress_headers - progress_refs).empty?
progress_headers.each do |reference|
  name = reference.split("/").last
  abort "generic progress listener headers must remain optional" unless contract.dig("components", "parameters", name, "required") == false
end

ask = schemas.fetch("PhoneAsk")
ask_create = schemas.fetch("CreatePhoneAskRequest")
ask_page = schemas.fetch("AgentAskPage")
event = schemas.fetch("ReportEventRequest")
progress = schemas.fetch("ProgressRequest")
device = schemas.fetch("DeviceRegistration")
listener_registration = schemas.fetch("ListenerRegistrationRequest")
listener_heartbeat_schema = schemas.fetch("ListenerHeartbeatRequest")
listener_lease = schemas.fetch("ListenerLease")
listener_release = schemas.fetch("ListenerDisconnectResponse")

[health, readiness, agent, ask, ask_create, ask_page, event, progress, device, listener_registration, listener_heartbeat_schema, listener_lease, listener_release].each do |schema|
  abort "protocol schemas must reject unknown fields" unless schema["additionalProperties"] == false
end
abort "health must not claim live schema compatibility" if health.fetch("properties").keys.any? { |field| field.start_with?("schema_") }
abort "readiness must expose live 0022 compatibility" unless readiness.dig("properties", "schema_0022_compatible", "type") == "boolean"
required_migrations = readiness.dig("properties", "required_migrations", "prefixItems").map { |item| item.fetch("const") }
abort "readiness migration sequence drift" unless required_migrations == %w[0017 0018 0019 0020 0021 0022]
abort "readiness migration cardinality drift" unless readiness.dig("properties", "required_migrations", "minItems") == 6 && readiness.dig("properties", "required_migrations", "maxItems") == 6
listener_string_fields = %w[listener_binding_id listener_lease_id listener_chat_id listener_chat_title listener_expires_at binding_id lease_id target_chat_id]
listener_string_fields.each do |field|
  abort "Agent missing nullable #{field}" unless agent.dig("properties", field, "type").sort == ["null", "string"]
end
%w[listener_generation generation].each do |field|
  abort "Agent missing nullable positive #{field}" unless agent.dig("properties", field, "type").sort == ["integer", "null"] && agent.dig("properties", field, "minimum") == 1
end
abort "Agent listening marker drift" unless agent.dig("properties", "listening", "type") == "boolean"
abort "Agent listener projection is not required" unless (%w[listening listener_binding_id listener_lease_id listener_generation listener_chat_id listener_chat_title listener_expires_at binding_id lease_id generation target_chat_id] - agent.fetch("required")).empty?
%w[client_turn_id binding_id lease_id generation target_chat_id].each do |field|
  abort "CreatePhoneAskRequest missing required #{field}" unless ask_create.fetch("required").include?(field)
end
%w[client_turn_id conversation_id lease_id listener_generation claim_token claim_deadline claim_generation answered_at reply_event_id answerable receipt].each do |field|
  abort "PhoneAsk missing protocol field #{field}" unless ask.fetch("properties").key?(field)
end
%w[ask_id claim_token generation].each do |field|
  abort "ReportEventRequest missing answer field #{field}" unless event.fetch("properties").key?(field)
end
%w[ask_id claim_token listener_generation].each do |field|
  abort "ProgressRequest missing Ask fence field #{field}" unless progress.fetch("properties").key?(field)
end
abort "ProgressRequest listener_generation must be positive" unless progress.dig("properties", "listener_generation", "minimum") == 1
abort "ios_simulator contract support drift" unless device.dig("properties", "platform", "enum").include?("ios_simulator")
apns_token = device.dig("properties", "push_token")
abort "APNs token must remain nullable for simulator registration" unless apns_token["type"] == ["string", "null"]
abort "APNs token length bounds drift" unless apns_token["minLength"] == 32 && apns_token["maxLength"] == 512
apns_pattern = Regexp.new(apns_token.fetch("pattern"))
abort "APNs token pattern must accept 32 even hex characters" unless apns_pattern.match?("a0" * 16)
abort "APNs token pattern must accept 512 even hex characters" unless apns_pattern.match?("B1" * 256)
abort "APNs token pattern accepted odd length" if apns_pattern.match?("a" * 33)
abort "APNs token pattern accepted non-hex input" if apns_pattern.match?(("a0" * 15) + "zz")
abort "listener renew must require exact lease generation" unless listener_heartbeat_schema.fetch("required").sort == %w[generation lease_id]
%w[lease_id generation renew_after_ms].each do |field|
  abort "ListenerLease missing #{field}" unless listener_lease.fetch("required").include?(field)
end
release_fields = %w[ok released binding_id agent_id chat_id listener_instance_id lease_id generation released_at]
abort "listener release response fields drift" unless listener_release.fetch("required").sort == release_fields.sort
abort "listener release ok marker drift" unless listener_release.dig("properties", "ok", "const") == true
abort "listener release completion marker drift" unless listener_release.dig("properties", "released", "const") == true
abort "listener release timestamp must be date-time" unless listener_release.dig("properties", "released_at", "format") == "date-time"

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
