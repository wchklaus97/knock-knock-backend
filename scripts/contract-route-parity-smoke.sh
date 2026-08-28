#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_SOURCE="${ROOT_DIR}/src/lib.rs"
CONTRACT="${ROOT_DIR}/contracts/openapi.yaml"

RUST_SOURCE="${RUST_SOURCE}" CONTRACT="${CONTRACT}" ruby <<'RUBY'
require "set"
require "yaml"

source = File.read(ENV.fetch("RUST_SOURCE"))

# The dispatch table is deliberately the executable route inventory. Literal
# segments remain literal; identifier segments become OpenAPI path params.
routes = source.scan(/\(Method::(Get|Post|Patch|Put|Delete),\s*\[([^\]]+)\]\)/m).map do |method, segments|
  path = segments.scan(/"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*)/).map do |literal, identifier|
    literal || "{#{identifier}}"
  end.join("/")
  [method.downcase, "/#{path}"]
end.to_set

# These routes return before the dispatch table because they do not need D1.
routes.merge([
  ["get", "/health"],
  ["get", "/ready"],
  ["get", "/v1/health"],
  ["get", "/metrics"]
])

contract = YAML.load_file(ENV.fetch("CONTRACT"))
paths = contract.fetch("paths")
abort "OpenAPI paths must be an object" unless paths.is_a?(Hash)

http_methods = %w[get post patch put delete].freeze
contract_routes = paths.flat_map do |path, path_item|
  abort "OpenAPI path item #{path} must be an object" unless path_item.is_a?(Hash)

  path_item.each_with_object([]) do |(method, operation), route_list|
    next unless http_methods.include?(method)

    unless operation.is_a?(Hash)
      abort "OpenAPI operation #{method.upcase} #{path} must be a non-null object"
    end
    operation_id = operation["operationId"]
    unless operation_id.is_a?(String) && !operation_id.empty?
      abort "OpenAPI operation #{method.upcase} #{path} requires a non-empty operationId"
    end
    responses = operation["responses"]
    unless responses.is_a?(Hash) && !responses.empty?
      abort "OpenAPI operation #{method.upcase} #{path} requires a non-empty responses object"
    end

    route_list << [method, path]
  end
end.to_set

register_operation = paths.fetch("/v1/auth/register").fetch("post")
unless register_operation["tags"] == ["Auth"] &&
    register_operation["security"] == [] &&
    register_operation.dig("requestBody", "required") == true &&
    register_operation.dig("requestBody", "content", "application/json", "schema", "$ref") == "#/components/schemas/AuthCredentials" &&
    register_operation["responses"].key?("200") &&
    register_operation["responses"].key?("default")
  abort "OpenAPI POST /v1/auth/register is malformed"
end

missing = routes - contract_routes
extra = contract_routes - routes

unless missing.empty? && extra.empty?
  warn "route/contract parity failed"
  warn "missing from OpenAPI: #{missing.to_a.sort.inspect}" unless missing.empty?
  warn "documented but not dispatched: #{extra.to_a.sort.inspect}" unless extra.empty?
  exit 1
end

required_memory_routes = Set[
  ["get", "/v1/phone/memories"],
  ["post", "/v1/phone/memories"],
  ["get", "/v1/phone/memories/{memory_id}"],
  ["delete", "/v1/phone/memories/{memory_id}"]
]
unless required_memory_routes.subset?(routes) && required_memory_routes.subset?(contract_routes)
  abort "structured Memory routes must remain executable and canonical"
end

operation_ids = contract.fetch("paths").flat_map do |_path, operations|
  operations.each_with_object([]) do |(method, operation), ids|
    ids << operation["operationId"] if http_methods.include?(method)
  end
end
unless operation_ids.uniq.length == operation_ids.length
  abort "OpenAPI operationId values must be unique"
end

puts "contract route parity smoke passed: #{routes.length} executable operations match OpenAPI"
RUBY
