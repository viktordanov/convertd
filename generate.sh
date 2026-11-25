#!/usr/bin/env bash
# Single entry point for everything generated from api/openapi.yaml.
# Run it after any change to the spec, then commit the regenerated files with it.
set -euo pipefail
cd "$(dirname "$0")"

# Pinned so that every machine generates the same code.
OAPI_CODEGEN_VERSION="v2.9.0"

echo "Checking the OpenAPI spec..."
python3 scripts/check_spec.py api/openapi.yaml

echo "Generating the Go client (oapi-codegen ${OAPI_CODEGEN_VERSION})..."
(
  cd client
  go run "github.com/oapi-codegen/oapi-codegen/v2/cmd/oapi-codegen@${OAPI_CODEGEN_VERSION}" \
    -config oapi-codegen.yaml ../api/openapi.yaml
  go mod tidy
  go build ./...
  go vet ./...
  go test ./...
)

echo "Done. Commit client/internal/api/client.go together with api/openapi.yaml."
