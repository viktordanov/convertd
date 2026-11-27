#!/usr/bin/env bash
# docker/corpus-check.sh <image> <corpus-bench-binary>
# Runs `corpus-bench check` (tools/corpus/, every case of corpus/expected.json) against <image>.
# The image job of .github/workflows/image.yml runs it on both architectures.
#
# The service runs as the manifests run it: no network, read-only root, /tmp only, uid 10001,
# no capabilities. The harness runs in HARNESS_IMAGE (a pinned glibc image, tag@sha256:...)
# inside the service's network namespace, because LibreOffice connects to the loopback address
# of its own container: only there does the canary listener see a connection
# (tools/corpus/README.md). Run it from the repository root, after
# `corpus/generate.py corpus/out --hostile`. The binary must be built for the image's arch.
# It exits with the status of `check`, and with 1 if the service logged an ERROR line.
set -euo pipefail

image="${1:?image}"; bench="${2:?corpus-bench binary}"
harness="${HARNESS_IMAGE:?set HARNESS_IMAGE to a pinned glibc image, tag@sha256:...}"
[[ -f corpus/expected.json && -d corpus/out ]] \
  || { echo "corpus-check: run from the repository root, after generate.py --hostile" >&2; exit 1; }
bench="$(cd "$(dirname "${bench}")" && pwd)/$(basename "${bench}")"
name="rs-convert-corpus-$$"
key="corpus-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"

# shellcheck disable=SC2317,SC2329  # called through the EXIT trap (SC2329 is ShellCheck 0.11)
cleanup() { docker rm -f "${name}" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker run -d --name "${name}" \
  --network none --read-only --tmpfs /tmp:rw,size=4g,mode=1777 \
  --user 10001:10001 --cap-drop ALL --security-opt no-new-privileges \
  --cpus 2 --memory 4g \
  -e API_KEY="${key}" -e OFFICE_WORKERS=1 -e RENDER_WORKERS=2 \
  "${image}" serve >/dev/null
ready=0
for _ in $(seq 1 120); do
  if docker exec "${name}" convertd healthcheck >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.5
done
if [[ "${ready}" != 1 ]]; then
  echo "corpus-check: not ready after 60 s" >&2; docker logs "${name}" 2>&1 | tail -n 50 >&2; exit 1
fi

rc=0
docker run --rm --network "container:${name}" \
  -v "${PWD}:/src:ro" -v "${bench}:/usr/local/bin/corpus-bench:ro" -w /src \
  -e CONVERT_API_KEY="${key}" \
  "${harness}" corpus-bench check --target http://127.0.0.1:8080 || rc=$?

errors="$(docker logs "${name}" 2>&1 | grep -c '"level":"ERROR"' || true)"
echo "corpus-check: check exit ${rc}; ${errors} ERROR lines in the service log"
if [[ "${errors}" != 0 ]]; then
  docker logs "${name}" 2>&1 | grep '"level":"ERROR"' | tail -n 20
  [[ "${rc}" != 0 ]] || rc=1
fi
exit "${rc}"
