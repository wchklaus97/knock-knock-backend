#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT="${ROOT_DIR}/contracts/openapi.yaml"
BASELINE="${ROOT_DIR}/contracts/v1-compatibility-baseline.json"

CONTRACT="$CONTRACT" BASELINE="$BASELINE" ruby <<'RUBY'
require "json"
require "yaml"

contract = YAML.load_file(ENV.fetch("CONTRACT"))
baseline = JSON.parse(File.read(ENV.fetch("BASELINE")))
errors = []

unless contract.fetch("openapi") == baseline.fetch("openapi")
  errors << "OpenAPI major/minor version changed from #{baseline.fetch("openapi")}"
end

paths = contract.fetch("paths", {})
baseline.fetch("paths").each do |path, methods|
  methods.each do |method|
    unless paths.dig(path, method)
      errors << "removed operation: #{method.upcase} #{path}"
    end
  end
end

schemas = contract.dig("components", "schemas") || {}
baseline.fetch("schemas").each do |name, expected|
  actual = schemas[name]
  if actual.nil?
    errors << "removed schema: #{name}"
    next
  end

  expected.fetch("required", []).each do |property|
    unless Array(actual["required"]).include?(property)
      errors << "removed required property: #{name}.#{property}"
    end
  end

  expected.fetch("properties", {}).each do |property, rules|
    actual_property = actual.dig("properties", property)
    if actual_property.nil?
      errors << "removed property: #{name}.#{property}"
      next
    end
    if rules["type"] && actual_property["type"] != rules["type"]
      errors << "changed type: #{name}.#{property} expected #{rules["type"].inspect}, got #{actual_property["type"].inspect}"
    end
    if rules.key?("const") && actual_property["const"] != rules["const"]
      errors << "changed const: #{name}.#{property}"
    end
    if rules["ref"] && actual_property["$ref"] != rules["ref"]
      errors << "changed ref: #{name}.#{property}"
    end
    expected_enum = rules["enum"]
    actual_enum = actual_property["enum"]
    if expected_enum && (!actual_enum || (expected_enum - actual_enum).any?)
      errors << "removed enum value: #{name}.#{property}"
    end
    if rules["required"]
      rules["required"].each do |nested_property|
        unless Array(actual_property["required"]).include?(nested_property)
          errors << "removed nested required property: #{name}.#{property}.#{nested_property}"
        end
      end
    end
  end
end

if errors.empty?
  puts "contract breaking smoke passed: v1 operations and compatibility-critical schema fields are preserved"
else
  warn errors.join("\n")
  exit 1
end
RUBY
