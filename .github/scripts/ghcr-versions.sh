#!/usr/bin/env bash
# ghcr-versions.sh: prints the X.Y.Z tags that a GHCR repository holds, one
# per line, sorted by version. A 404 on the first page means no repository yet, and prints
# nothing. Any other failure exits non-zero, so a release never decides on a partial list:
# a non-200 page (a 404 on a later page too), a page without a "tags" list, or a bad Link.
#
# Usage: ghcr-versions.sh <owner>/<name>
# Env: GHCR_USER and GHCR_TOKEN (the workflow's actor and GITHUB_TOKEN). Without them the
# token is anonymous, which reads public repositories only (for a local check). GHCR_SCOPE
# (default pull) is the token's scope; the release asks for pull,push, as docker push does,
# so that a repository that does not exist yet answers 404 rather than denying the token.
set -euo pipefail

name="${1:?usage: ghcr-versions.sh <owner>/<name>}"

auth=()
[[ -n "${GHCR_TOKEN:-}" ]] && auth=(-u "${GHCR_USER:?}:${GHCR_TOKEN}")
token="$(curl -fsS --retry 3 ${auth[@]+"${auth[@]}"} \
  "https://ghcr.io/token?service=ghcr.io&scope=repository:${name}:${GHCR_SCOPE:-pull}" | jq -er .token)"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
url="https://ghcr.io/v2/${name}/tags/list?n=1000"
first=1
: > "${work}/tags"
while [[ -n "${url}" ]]; do
  status="$(curl -sS --retry 3 -o "${work}/body" -D "${work}/headers" -w '%{http_code}' \
    -H "Authorization: Bearer ${token}" "${url}")"
  if [[ "${status}" == 404 && "${first}" == 1 ]]; then
    break           # no repository yet: no versions
  fi
  if [[ "${status}" != 200 ]]; then
    echo "ghcr-versions.sh: ${url} answered ${status}: $(head -c 300 "${work}/body")" >&2
    exit 1
  fi
  first=0
  # A page must name a "tags" list (null for an empty repository); anything else is not
  # a listing.
  if ! jq -e 'type == "object" and has("tags") and (.tags == null or (.tags | type == "array"))' \
       "${work}/body" >/dev/null; then
    echo "ghcr-versions.sh: ${url} gave no tag list: $(head -c 300 "${work}/body")" >&2
    exit 1
  fi
  jq -r '(.tags // [])[]' "${work}/body" >> "${work}/tags"
  # The next page, from a Link header: <path?last=…&n=…>; rel="next".
  next="$(tr -d '\r' < "${work}/headers" | sed -n 's/^[Ll]ink: *<\([^>]*\)>; *rel="next".*/\1/p' | head -n1)"
  url=""
  if [[ -n "${next}" ]]; then
    [[ "${next}" == /* ]] && url="https://ghcr.io${next}" || url="${next}"
  fi
done
grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' "${work}/tags" | sort -uV || true
