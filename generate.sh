#!/usr/bin/env bash
# Single entry point for everything generated from the API spec.
#
#   ./generate.sh           validate the spec, regenerate the Go client and test
#                           it; then commit the spec and client/internal/api/client.go
#                           together, and any facade change separately
#   ./generate.sh --check   CI: the same, then fail if any generated file differs
#                           from what is in the working tree or in HEAD. On drift
#                           the working tree is put back as it was.
#
# Needs go 1.26 or later and git; cargo too once SPEC_SOURCE is "emitted".
# CARGO_TARGET_DIR and GOCACHE are taken from the caller's environment.
set -euo pipefail
cd "$(dirname "$0")"

CHECK=false
case "${1:-}" in
  --check) CHECK=true ;;
  "") ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

# Pinned so that every machine generates the same code.
OAPI_CODEGEN_VERSION="v2.9.0"

# "emitted": api/openapi.json comes from `convertd openapi` (ledger 004 plan,
# step 8). The hand-written api/openapi.yaml stays as the reference that the
# emitted spec is compared with, until it is retired (step 11).
SPEC_SOURCE="emitted"

GENERATED=(client/internal/api/client.go client/go.mod client/go.sum)
if [[ "$SPEC_SOURCE" == "emitted" ]]; then
  SPEC="api/openapi.json"
  GENERATED+=("$SPEC")
else
  SPEC="api/openapi.yaml"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# In --check mode, keep a copy of every generated file as it is now, so that
# drift is measured against the working tree and the tree can be put back.
if $CHECK; then
  for f in "${GENERATED[@]}"; do
    if [[ -f "$f" ]]; then
      mkdir -p "$tmp/before/$(dirname "$f")"
      cp "$f" "$tmp/before/$f"
    fi
  done
fi

speccheck() {
  (cd client/contract && go run ./cmd/speccheck "$@")
}

if [[ "$SPEC_SOURCE" == "emitted" ]]; then
  echo "Emitting the spec from convertd..."
  cargo run --locked --quiet --bin convertd -- openapi > "$tmp/a.json"
  cargo run --locked --quiet --bin convertd -- openapi > "$tmp/b.json"
  if ! cmp -s "$tmp/a.json" "$tmp/b.json"; then
    echo "convertd openapi is not deterministic: two runs differ" >&2
    diff "$tmp/a.json" "$tmp/b.json" | head -40 >&2
    exit 1
  fi
  cp "$tmp/a.json" "$SPEC"
fi

echo "Checking ${SPEC}..."
# Paths are relative to client/contract, where speccheck runs.
speccheck validate "../../${SPEC}"

if [[ "$SPEC_SOURCE" == "emitted" && -f api/openapi.yaml ]]; then
  echo "Comparing the emitted spec with the reference..."
  speccheck compare ../../api/openapi.json ../../api/openapi.yaml
fi

echo "Generating the Go client (oapi-codegen ${OAPI_CODEGEN_VERSION})..."
(
  cd client
  go run "github.com/oapi-codegen/oapi-codegen/v2/cmd/oapi-codegen@${OAPI_CODEGEN_VERSION}" \
    -config oapi-codegen.yaml "../${SPEC}"
  go mod tidy
  go build ./...
  go vet ./...
  go test -race ./...
)

echo "Compiling the contract module against the new client..."
# The contract tests import client/internal/api, so they must still build.
# Running the live cases needs a server: see scripts/contract_test.sh.
(cd client/contract && go vet -tags contract ./...)

if ! $CHECK; then
  echo "Done. Commit ${SPEC} and client/internal/api/client.go together;"
  echo "put any facade change in a separate commit."
  exit 0
fi

echo "Checking for drift..."
drift=()
for f in "${GENERATED[@]}"; do
  if [[ ! -f "$tmp/before/$f" ]] || ! cmp -s "$tmp/before/$f" "$f"; then
    drift+=("$f")
  fi
done
if ((${#drift[@]} > 0)); then
  echo "These files differ from what generation produces:" >&2
  for f in "${drift[@]}"; do
    echo "  $f" >&2
    if [[ -f "$tmp/before/$f" ]]; then
      diff -u "$tmp/before/$f" "$f" | head -20 >&2 || true
    fi
  done
  for f in "${drift[@]}"; do
    if [[ -f "$tmp/before/$f" ]]; then cp "$tmp/before/$f" "$f"; else rm -f "$f"; fi
  done
  echo "Run ./generate.sh and commit the result." >&2
  exit 1
fi

if ! git diff --quiet HEAD -- "${GENERATED[@]}"; then
  echo "Generated files match the spec but are not committed:" >&2
  git diff --stat HEAD -- "${GENERATED[@]}" >&2
  exit 1
fi

untracked="$(git ls-files --others --exclude-standard -- api client)"
if [[ -n "$untracked" ]]; then
  echo "Untracked files under api/ or client/:" >&2
  echo "$untracked" >&2
  exit 1
fi

echo "No drift."
