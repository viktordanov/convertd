#!/usr/bin/env bash
# Runs the contract tests in client/contract against a real convertd.
#
#   scripts/contract_test.sh [-- GO_TEST_ARGS...]
#       Binary mode: builds convertd with --features test-hooks and lets the
#       tests start it with the fake engine (TEST_ENGINE=fake).
#   scripts/contract_test.sh --image REF [-- GO_TEST_ARGS...]
#       Image mode: starts the image REF as container rs-convert-004-contract
#       on a loopback port, runs the tests against it and removes it.
#
# Arguments after -- go to `go test`, for example `-- -run TestConvert -v`.
# CARGO_TARGET_DIR and GOCACHE are taken from the caller's environment.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=""
GO_ARGS=()
while (($# > 0)); do
  case "$1" in
    --image)
      [[ $# -ge 2 ]] || { echo "usage: $0 [--image REF] [-- GO_TEST_ARGS...]" >&2; exit 2; }
      IMAGE="$2"
      shift 2
      ;;
    --)
      shift
      GO_ARGS=("$@")
      break
      ;;
    *)
      echo "usage: $0 [--image REF] [-- GO_TEST_ARGS...]" >&2
      exit 2
      ;;
  esac
done

# Runs the tests verbosely into a log. On success it prints only the skipped
# cases with their reasons, the coverage report and the package lines; on
# failure it prints the whole log.
run_tests() {
  local log status=0
  log="$(mktemp)"
  (cd client/contract && go test -tags contract -count=1 -race -v ${GO_ARGS[@]+"${GO_ARGS[@]}"} ./...) >"$log" 2>&1 || status=$?
  if ((status != 0)); then
    cat "$log"
  else
    grep -E -B1 -- '--- SKIP' "$log" | grep -v -E -- '^(--$|=== |\s+--- PASS)' || true
    grep -E 'FINDING' "$log" | sort -u || true
    grep -E '^(coverage:|  [A-Za-z]+ [0-9]{3}$|ok |FAIL)' "$log" || true
  fi
  rm -f "$log"
  return "$status"
}

if [[ -z "$IMAGE" ]]; then
  echo "Building convertd with --features test-hooks..."
  cargo build --locked --features test-hooks --bin convertd
  target_dir="${CARGO_TARGET_DIR:-target}"
  [[ "$target_dir" == /* ]] || target_dir="$(pwd)/$target_dir"
  bin="$target_dir/debug/convertd"
  [[ -x "$bin" ]] || { echo "convertd not found at $bin" >&2; exit 1; }
  echo "Running the contract tests against $bin..."
  CONVERTD_BIN="$bin" run_tests
  exit 0
fi

CONTAINER="rs-convert-004-contract"
API_KEY="contract-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup

echo "Starting $IMAGE as $CONTAINER..."
docker run -d --name "$CONTAINER" -e API_KEY="$API_KEY" -p 127.0.0.1::8080 "$IMAGE" >/dev/null
port="$(docker port "$CONTAINER" 8080/tcp | head -1 | sed 's/.*://')"
host="http://127.0.0.1:${port}"

echo "Waiting for $host/readyz..."
for _ in $(seq 1 120); do
  if curl -fsS -o /dev/null "$host/readyz"; then
    break
  fi
  sleep 1
done
curl -fsS -o /dev/null "$host/readyz" || {
  echo "the container did not become ready; its log:" >&2
  docker logs "$CONTAINER" >&2 || true
  exit 1
}

echo "Running the contract tests against $host..."
if ! CONVERT_HOST="$host" CONVERT_API_KEY="$API_KEY" run_tests; then
  docker logs "$CONTAINER" 2>&1 | tail -100 >&2 || true
  exit 1
fi
