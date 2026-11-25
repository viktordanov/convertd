#!/usr/bin/env bash
# build_docker.sh (ledger item 003). First proved in plan step 6.
# Builds the rs-convert image with docker buildx and pushes it as <repo>:v<x.y.z>.
#
# Usage:
#   ./build_docker.sh <repo> [options]
#     <repo>                 image repository, e.g. registry.example.com/rs-convert
#   Options:
#     --platform <list>      default linux/amd64,linux/arm64
#     --cjk                  add Noto CJK fonts; the tag gets a -cjk suffix
#     --load                 load into the local Docker instead of pushing (one platform only)
#     --dev                  allow an untagged or dirty tree; the tag becomes v<x.y.z>-dev.<sha>
#     --force                overwrite a tag that already exists in the registry
#
# The version comes from Cargo.toml. A release build (no --dev) requires a clean tree whose
# HEAD carries the git tag v<x.y.z>. Tags are never moved silently.
set -euo pipefail
cd "$(dirname "$0")"

die() { echo "build_docker.sh: $*" >&2; exit 1; }

repo="${1:-}"; [[ -n "${repo}" && "${repo}" != -* ]] || die "usage: ./build_docker.sh <repo> [options]"
shift
platforms="linux/amd64,linux/arm64"
cjk=0; load=0; dev=0; force=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --platform) platforms="${2:?}"; shift 2 ;;
    --cjk) cjk=1; shift ;;
    --load) load=1; shift ;;
    --dev) dev=1; shift ;;
    --force) force=1; shift ;;
    *) die "unknown option $1" ;;
  esac
done

version="$(sed -n '/^\[package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -n1)"
[[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "no x.y.z version in Cargo.toml [package]"
commit="$(git rev-parse HEAD)"
build_date="$(TZ=UTC git log -1 --date=format-local:%Y-%m-%dT%H:%M:%SZ --format=%cd)"

tag="v${version}"
if [[ "${dev}" == 1 ]]; then
  tag="${tag}-dev.$(git rev-parse --short=12 HEAD)"
  git diff --quiet HEAD || tag="${tag}.dirty"
else
  git diff --quiet HEAD || die "the tree has uncommitted changes (use --dev for a test build)"
  git tag --points-at HEAD | grep -x "v${version}" >/dev/null || die "HEAD is not tagged v${version}"
fi
[[ "${cjk}" == 1 ]] && tag="${tag}-cjk"
ref="${repo}:${tag}"

n_platforms="$(tr ',' '\n' <<<"${platforms}" | wc -l | tr -d ' ')"
# Both platforms build at once inside a ~4 GB Docker VM: cap cargo per platform.
jobs=4; [[ "${n_platforms}" -gt 1 ]] && jobs=2

if [[ "${load}" == 1 ]]; then
  [[ "${n_platforms}" == 1 ]] || die "--load needs exactly one --platform"
  output="type=docker,name=${ref}"
else
  if [[ "${force}" != 1 ]] && docker buildx imagetools inspect "${ref}" >/dev/null 2>&1; then
    die "${ref} already exists (use --force to overwrite)"
  fi
  output="type=image,name=${ref},push=true"
fi

builder="rs-convert-builder"
docker buildx inspect "${builder}" >/dev/null 2>&1 \
  || docker buildx create --name "${builder}" --driver docker-container --bootstrap >/dev/null

attest=()
[[ "${load}" == 1 ]] || attest=(--provenance=mode=max --sbom=true)

echo "building ${ref} for ${platforms} (commit ${commit:0:12}, cargo -j ${jobs})"
docker buildx build \
  --builder "${builder}" \
  --file docker/Dockerfile \
  --target runtime \
  --platform "${platforms}" \
  --build-arg CONVERTD_VERSION="${version}" \
  --build-arg CONVERTD_COMMIT="${commit}" \
  --build-arg CONVERTD_BUILD_DATE="${build_date}" \
  --build-arg FONTS_CJK="${cjk}" \
  --build-arg CARGO_JOBS="${jobs}" \
  ${attest[@]+"${attest[@]}"} \
  --output "${output}" \
  .

echo "done: ${ref}"
[[ "${load}" == 1 ]] || docker buildx imagetools inspect "${ref}" | sed -n '1,12p'
