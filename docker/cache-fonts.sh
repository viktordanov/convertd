#!/usr/bin/env bash
# docker/cache-fonts.sh, installed as /opt/convertd/cache-fonts.sh. Run it as root.
# Builds the system fontconfig cache so that it stays valid after any image export.
#
# fontconfig trusts a cache file only if the mtime of its font directory matches the stored
# value in seconds AND nanoseconds. A layer that passes through a tar file keeps whole seconds
# only: a docker-container buildx builder, the GitHub Actions cache and a registry push all do
# this. The baked cache is then stale for every directory, and each worker writes a user cache
# under /tmp at start. So this script sets every font directory to a whole-second mtime first,
# then runs fc-cache, then proves that the cache is valid for an unprivileged user.
#
# install-engines.sh calls it. A derived image that adds fonts calls it again, then
# write-build-info.sh.
set -euo pipefail

# The directories that fontconfig scans, from its own configuration.
mapfile -t dirs < <(fc-cache -v --really-force 2>/dev/null \
  | sed -n '/^Font directories:/,/^[^[:space:]]/{s/^[[:space:]]\{1,\}//p;}')
[[ "${#dirs[@]}" -gt 0 ]] || { echo "cache-fonts: fc-cache listed no font directories" >&2; exit 1; }

stamp="@$(date +%s)"
present=0
for d in "${dirs[@]}"; do
  if [[ -d "${d}" ]]; then touch -m -d "${stamp}" "${d}"; present=$((present + 1)); fi
done

fc-cache -s -f

# Every directory must keep a whole-second mtime, or an export breaks the cache again.
for d in "${dirs[@]}"; do
  if [[ -d "${d}" && "$(stat -c %y "${d}")" != *.000000000* ]]; then
    echo "cache-fonts: ${d} has a sub-second mtime after fc-cache" >&2; exit 1
  fi
done

# An unprivileged user cannot write /var/cache/fontconfig. If it writes a user cache, a
# system cache file is stale or missing.
check="$(mktemp -d)"
chmod 0777 "${check}"
setpriv --reuid=65534 --regid=65534 --clear-groups \
  env HOME="${check}" XDG_CACHE_HOME="${check}/cache" fc-list >/dev/null
stale="$(find "${check}" -name '*.cache-*' | wc -l)"
rm -rf "${check}"
if [[ "${stale}" != 0 ]]; then
  echo "cache-fonts: ${stale} font directories are not in the system cache" >&2; exit 1
fi
echo "cache-fonts: ${present} of ${#dirs[@]} configured font directories exist; all cached with whole-second mtimes"
