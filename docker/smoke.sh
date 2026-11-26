#!/usr/bin/env bash
# docker/smoke.sh (ledger item 003). The image job runs it with docker/smoke/small.docx.
# Proves the image, not the conversions (those belong to lane 005's corpus runs):
#   1. image size against a budget
#   2. time from `docker run` to /readyz 200
#   3. one DOCX to PDF over HTTP: curl exits 0, the body has its Content-Length, ends with
#      %%EOF and has the expected Convert-Page-Count; no job-<hex> directory is left after
#   4. the baked fontconfig cache is used (no user cache written)
# The container runs as the manifests run it: no network, read-only root, writable /tmp only,
# UID 10001, no capabilities. The curl client joins the container's network namespace, so it
# reaches the service on loopback and nothing reaches the outside.
# Linux only (GNU date for the timer).
#
# Usage: docker/smoke.sh <image> <docx-file> [max-image-mb] [expected-pages]
#   for example: docker/smoke.sh rs-convert:ci docker/smoke/small.docx 635
#   expected-pages defaults to 2, the page count of docker/smoke/small.docx (corpus entry
#   docx-small in corpus/expected.json).
# Sizes are in MB of 10^6 bytes, as docker/README.md gives them.
set -euo pipefail

image="${1:?image}"; docx="${2:?docx file}"; max_mb="${3:-0}"; want_pages="${4:-2}"
curl_image="${CURL_IMAGE:?set CURL_IMAGE to a pinned curl image, tag@sha256:...}"
dir="$(cd "$(dirname "${docx}")" && pwd)"; file="$(basename "${docx}")"
name="rs-convert-003-smoke-$$"
out="$(mktemp -d)"
key="smoke-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
fail=0

# shellcheck disable=SC2317,SC2329  # called through the EXIT trap (SC2329 is ShellCheck 0.11)
cleanup() { docker rm -f "${name}" >/dev/null 2>&1 || true; rm -rf "${out}"; }
trap cleanup EXIT
note() { printf '%-26s %s\n' "$1" "$2"; }
bad()  { note "$1" "FAIL $2"; fail=1; }

# 1. Size.
size_mb=$(( $(docker image inspect -f '{{.Size}}' "${image}") / 1000 / 1000 ))
note "image size" "${size_mb} MB"
if [[ "${max_mb}" -gt 0 && "${size_mb}" -gt "${max_mb}" ]]; then bad "size budget" "> ${max_mb} MB"; fi

# 2. Time to ready.
t0=$(date +%s%N)
docker run -d --name "${name}" \
  --network none --read-only --tmpfs /tmp:rw,size=2g,mode=1777 \
  --user 10001:10001 --cap-drop ALL --security-opt no-new-privileges \
  --cpus 2 --memory 4g \
  -e API_KEY="${key}" -e OFFICE_WORKERS=1 -e RENDER_WORKERS=2 \
  -e TMP_MAX_BYTES=1073741824 \
  "${image}" serve >/dev/null
ready=0
for _ in $(seq 1 120); do
  if docker exec "${name}" convertd healthcheck >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.5
done
t1=$(date +%s%N)
if [[ "${ready}" == 1 ]]; then note "time to ready" "$(( (t1 - t0) / 1000000 )) ms"
else bad "time to ready" "not ready after 60 s"; docker logs "${name}" | tail -n 50; exit 1; fi

# 3. One DOCX over HTTP. curl writes one '|'-separated line as the last line of its stderr:
# status, bytes received, Content-Length, Convert-Page-Count, total time, Server-Timing. The
# body (a PDF, or the error JSON) goes to out.pdf. A body cut short makes curl exit non-zero
# (18), so the exit code is part of the check.
rc=0
docker run --rm --network "container:${name}" -v "${dir}:/in:ro" "${curl_image}" \
  -sS -o - \
  -w '%{stderr}%{http_code}|%{size_download}|%header{content-length}|%header{convert-page-count}|%{time_total}|%header{server-timing}\n' \
  -H "Authorization: Bearer ${key}" \
  -F "file=@/in/${file}" -F 'options={"output":"pdf"};type=application/json' \
  http://127.0.0.1:8080/v1/convert 2>"${out}/status" >"${out}/out.pdf" || rc=$?
status=""; got=""; length=""; pages=""; total=""; timing=""
IFS='|' read -r status got length pages total timing < <(tail -n 1 "${out}/status") || true
# %%EOF ends every complete PDF; it must be in the last 1 KiB. grep -c reads all its input,
# so pipefail cannot fail it early.
eof="$(tail -c 1024 "${out}/out.pdf" | LC_ALL=C grep -a -c '%%EOF' || true)"
why=""
if [[ "${rc}" != 0 ]]; then why="curl exit ${rc}"
elif [[ "${status}" != 200 ]]; then why="status ${status:-none}"
elif [[ "$(head -c 5 "${out}/out.pdf")" != "%PDF-" ]]; then why="body is not a PDF"
elif [[ -z "${length}" || "${length}" != "${got}" ]]; then why="Content-Length ${length:-missing}, received ${got:-0} bytes"
elif [[ "${eof}" == 0 ]]; then why="no %%EOF in the last 1 KiB"
elif [[ "${pages}" != "${want_pages}" ]]; then why="Convert-Page-Count ${pages:-missing}, expected ${want_pages}"
fi
if [[ -z "${why}" ]]; then
  note "docx to pdf" "ok, ${got} bytes, ${pages} pages, in ${total} s"
  note "server-timing" "${timing:-none}"
else
  bad "docx to pdf" "${why}"
  head -c 2000 "${out}/out.pdf"; echo
  cat "${out}/status"
  docker logs "${name}" 2>&1 | tail -n 50
fi
# 001's layout: one job-<32hex> directory per job under TMP_DIR. The supervisor deletes it when
# the response ends, so the check waits up to 5 s.
left=""
for _ in $(seq 1 10); do
  left="$(docker exec "${name}" sh -c 'ls -d /tmp/convert/job-* 2>/dev/null | wc -l')"
  if [[ "${left}" == 0 ]]; then break; fi
  sleep 0.5
done
if [[ "${left}" == 0 ]]; then note "job dirs cleaned" "ok"; else bad "job dirs cleaned" "${left} left"; fi

# 4. The system font cache from the build is valid: no worker wrote a user cache.
# 002 puts each worker's XDG_CACHE_HOME in its slot under TMP_DIR.
fcu="$(docker exec "${name}" sh -c 'find /tmp -path "*fontconfig*" -name "*.cache-*" 2>/dev/null | wc -l')"
if [[ "${fcu}" == 0 ]]; then note "baked font cache used" "ok"
else bad "baked font cache used" "${fcu} user cache files"; fi

exit "${fail}"
