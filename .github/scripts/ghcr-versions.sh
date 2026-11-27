#!/usr/bin/env bash
# ghcr-versions.sh (ledger item 007): prints the X.Y.Z tags that a GHCR repository holds, one
# per line, sorted by version. A repository that does not exist yet prints nothing. Any
# other failure (auth, network, a bad answer) exits non-zero, so a release never decides on
# a partial list.
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
: > "${work}/tags"
while [[ -n "${url}" ]]; do
  status="$(curl -sS --retry 3 -o "${work}/body" -D "${work}/headers" -w '%{http_code}' \
    -H "Authorization: Bearer ${token}" "${url}")"
  case "${status}" in
    200) ;;
    404) break ;;   # no repository yet: no versions
    *) echo "ghcr-versions.sh: ${url} answered ${status}: $(head -c 300 "${work}/body")" >&2; exit 1 ;;
  esac
  jq -er '(.tags // [])[]' "${work}/body" >> "${work}/tags" || [[ "$(jq -r '.tags | length' "${work}/body")" == 0 ]]
  # The next page, from a Link header: <path?last=…&n=…>; rel="next".
  next="$(tr -d '\r' < "${work}/headers" | sed -n 's/^[Ll]ink: *<\([^>]*\)>; *rel="next".*/\1/p' | head -n1)"
  url=""
  if [[ -n "${next}" ]]; then
    [[ "${next}" == /* ]] && url="https://ghcr.io${next}" || url="${next}"
  fi
done
grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' "${work}/tags" | sort -uV || true
