#!/usr/bin/env bash
# docker/fetch-verified.sh <sha256> <out> <url>...
# Downloads one file from the first URL that gives the pinned SHA-256, and writes it to <out>.
# The engines stage of docker/Dockerfile runs it for every engine download.
#
# Every URL is tried in order. curl verifies TLS as usual and retries transient errors. A URL
# that fails, or that serves other bytes, is reported and skipped. The script fails only if no
# URL gives the pinned file. If <out> already holds the pinned file (a cache mount), nothing is
# downloaded.
#
# Why a list: download.documentfoundation.org redirects to a mirror near the client. A mirror
# can serve an incomplete certificate chain, which Debian's curl rejects with error 60 ("unable
# to get local issuer certificate"). The TDF archive host uses the same redirector, so it is not
# a fallback for that case. Fixed mirrors in other countries are.
set -euo pipefail
sha="${1:?sha256}"; out="${2:?output file}"; shift 2
[[ "${sha}" =~ ^[0-9a-f]{64}$ ]] || { echo "fetch-verified: not a SHA-256: '${sha}'" >&2; exit 1; }
[[ $# -gt 0 ]] || { echo "fetch-verified: no URL for ${out}" >&2; exit 1; }

matches() { echo "${sha}  $1" | sha256sum -c --status - 2>/dev/null; }

if [[ -f "${out}" ]] && matches "${out}"; then
  echo "fetch-verified: ${out} is cached"; exit 0
fi
part="${out}.part"
for url in "$@"; do
  rm -f "${part}"
  if ! curl -fsSL --retry 3 --retry-delay 2 --retry-connrefused --connect-timeout 20 \
        -o "${part}" "${url}"; then
    echo "fetch-verified: download failed, next URL: ${url}" >&2
    continue
  fi
  if ! matches "${part}"; then
    echo "fetch-verified: SHA-256 differs, next URL: ${url}" >&2
    continue
  fi
  mv "${part}" "${out}"
  echo "fetch-verified: ${out} from ${url}"
  exit 0
done
rm -f "${part}"
echo "fetch-verified: no URL gave SHA-256 ${sha} for ${out}" >&2
exit 1
