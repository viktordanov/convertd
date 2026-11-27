#!/usr/bin/env bash
# docker/ab-pages.sh <image-a> <image-b> <corpus-dir> [out-dir]
# Proves that two images render the same pages: the A/B gate for a size trim or an engine bump.
#
# Both images run as the manifests run them: no network, read-only root, /tmp only, uid
# 10001, no capabilities. A pinned curl container joins each one's network namespace for each
# request (as in smoke.sh), so a corpus file that links outside reaches nothing. Every file under
# <corpus-dir> goes to POST /v1/convert with output=images, PNG at 72 dpi. For each file the
# script records the HTTP status and, on 200, the SHA-256 of every page. PDF bytes differ
# between runs (IDs, dates), rendered pixels must not. It exits 1 if any line differs, or if
# any request failed in curl.
# Needs tar, zstd and sha256sum or shasum on the host, and CURL_IMAGE (tag@sha256:...).
set -euo pipefail

image_a="${1:?image a}"; image_b="${2:?image b}"; source_dir="$(cd "${3:?corpus dir}" && pwd)"
out="${4:-$(mktemp -d)}"
mkdir -p "${out}/x"
out="$(cd "${out}" && pwd)"
# The curl container runs as its own user. A checkout or a corpus written under umask 077 has
# 0600 files that it cannot read (curl exit 26), so it reads a copy with fixed modes.
corpus="${out}/corpus"
rm -rf "${out:?}/corpus"
cp -R "${source_dir}" "${corpus}"
chmod -R a+rX "${corpus}"
chmod 0755 "${out}"
# A reused out-dir must not carry the curl errors of an earlier run.
rm -f "${out:?}/errors.txt"
# The curl container runs as its own user and writes the response bodies here.
chmod 0777 "${out}/x"
curl_image="${CURL_IMAGE:?set CURL_IMAGE to a pinned curl image, tag@sha256:...}"
key="ab-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
names=()

# shellcheck disable=SC2317,SC2329  # called through the EXIT trap (SC2329 is ShellCheck 0.11)
cleanup() { for n in ${names[@]+"${names[@]}"}; do docker rm -f "${n}" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT

if command -v sha256sum >/dev/null; then sha() { sha256sum "$@"; }; else sha() { shasum -a 256 "$@"; }; fi

start() { # <image> <name>
  docker run -d --name "$2" --network none \
    --read-only --tmpfs /tmp:rw,size=2g,mode=1777 \
    --user 10001:10001 --cap-drop ALL --security-opt no-new-privileges \
    --cpus 2 --memory 4g \
    -e API_KEY="${key}" -e OFFICE_WORKERS=1 -e RENDER_WORKERS=2 -e TMP_MAX_BYTES=1073741824 \
    "$1" serve >/dev/null
}

wait_ready() { # <name>
  for _ in $(seq 1 120); do
    if docker exec "$1" convertd healthcheck >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  echo "ab-pages: $1 not ready after 60 s" >&2; docker logs "$1" 2>&1 | tail -n 30 >&2; return 1
}

run() { # <name> <label> -> writes ${out}/<label>.txt
  local name="$1" list="${out}/${2}.txt" i=0
  : > "${list}"
  while IFS= read -r -d '' f; do
    # A new body file per request: a file deleted on the host can still be visible through the
    # bind mount, and curl then fails to write it (exit 23).
    i=$((i + 1))
    local rel="${f#"${corpus}"/}" body_name="body-${2}-${i}" pages="${out}/pages"
    local body="${out}/x/${body_name}"
    rm -rf "${pages}"; mkdir -p "${pages}"
    local status rc=0
    status="$(docker run --rm --network "container:${name}" -v "${corpus}:/in:ro" -v "${out}/x:/x" \
      "${curl_image}" -sS -m 240 -o "/x/${body_name}" -w '%{http_code}' \
      -H "Authorization: Bearer ${key}" \
      -F "file=@/in/${rel}" \
      -F 'options={"output":"images","images":{"format":"png","dpi":72}};type=application/json' \
      http://127.0.0.1:8080/v1/convert 2>"${out}/err")" || rc=$?
    if [[ "${rc}" != 0 ]]; then
      printf '%s\t%s\tcurl exit %s: %s\n' "${2}" "${rel}" "${rc}" "$(tr '\n' ' ' < "${out}/err")" >> "${out}/errors.txt"
      status="curl-exit-${rc}"
    fi
    if [[ "${status}" == 200 ]] && zstd -dcq "${body}" | tar -xf - -C "${pages}" 2>/dev/null; then
      local n
      n="$(find "${pages}/pages" -type f 2>/dev/null | wc -l | tr -d ' ')"
      printf '%s\t200\t%s pages\n' "${rel}" "${n}" >> "${list}"
      (cd "${pages}" && find pages -type f | LC_ALL=C sort | while read -r p; do
         printf '%s\t%s\t%s\n' "${rel}" "${p}" "$(sha "${p}" | cut -d' ' -f1)"; done) >> "${list}"
    else
      printf '%s\t%s\n' "${rel}" "${status}" >> "${list}"
    fi
  done < <(find "${corpus}" -type f -print0 | LC_ALL=C sort -z)
}

name_a="rs-convert-ab-a-$$"; name_b="rs-convert-ab-b-$$"
names=("${name_a}" "${name_b}")
start "${image_a}" "${name_a}"
start "${image_b}" "${name_b}"
wait_ready "${name_a}"
wait_ready "${name_b}"
run "${name_a}" a
run "${name_b}" b

files="$(cut -f1 "${out}/a.txt" | LC_ALL=C sort -u | wc -l | tr -d ' ')"
pages="$(awk -F'\t' '$2 ~ /^pages\// {n++} END {print n + 0}' "${out}/a.txt")"
echo "ab-pages: ${files} files, ${pages} pages with ${image_a}; lists in ${out}"
# A request that curl could not make proves nothing about either image, even when both sides
# fail the same way, so any curl error fails the run.
if [[ -s "${out}/errors.txt" ]]; then
  echo "ab-pages: $(wc -l < "${out}/errors.txt" | tr -d ' ') requests failed in curl:" >&2
  head -n 20 "${out}/errors.txt" >&2
  exit 1
fi
if diff -u "${out}/a.txt" "${out}/b.txt" > "${out}/diff.txt"; then
  echo "ab-pages: identical: every status, page count and page hash matches"
else
  echo "ab-pages: DIFFERENT, see ${out}/diff.txt" >&2
  head -n 40 "${out}/diff.txt" >&2
  exit 1
fi
