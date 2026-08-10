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
  ["get", "/v1/health"],
  ["get", "/metrics"]
])

contract = YAML.load_file(ENV.fetch("CONTRACT"))
contract_routes = contract.fetch("paths").flat_map do |path, operations|
  operations.keys.grep(/^(get|post|patch|put|delete)$/).map { |method| [method, path] }
end.to_set

missing = routes - contract_routes
extra = contract_routes - routes

unless missing.empty? && extra.empty?
  warn "route/contract parity failed"
  warn "missing from OpenAPI: #{missing.to_a.sort.inspect}" unless missing.empty?
  warn "documented but not dispatched: #{extra.to_a.sort.inspect}" unless extra.empty?
  exit 1
end

operation_ids = contract.fetch("paths").flat_map do |_path, operations|
  operations.values.map { |operation| operation["operationId"] }.compact
end
unless operation_ids.uniq.length == operation_ids.length
  abort "OpenAPI operationId values must be unique"
end

puts "contract route parity smoke passed: #{routes.length} executable operations match OpenAPI"
RUBY
