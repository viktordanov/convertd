#!/usr/bin/env bash
# docker/write-build-info.sh, installed as /opt/convertd/write-build-info.sh.
# Writes /opt/convertd/build-info.json, the one source of engine versions and installed fonts
# for /v1/capabilities, the fingerprint and FONT_SUBSTITUTED (plans 001 and 005):
#   {"libreoffice": "26.8.3.1", "pdfium": "8179", "fonts": [...],
#    "font_substitutes": {"Calibri": "Carlito", ...}, "default_substitute": "...",
#    "coverage": [[32, 126], [160, 591], ...]}
# "font_substitutes" maps each family in the probe list (lane 005 owns it) to what fc-match picks
# in this image, aliases included; "default_substitute" is the fallback for any other name.
# "coverage" is the union of every face's charset as merged, sorted, inclusive code point
# ranges in decimal; GLYPHS_MISSING compares the document's text with it. The image has no
# Python, so POSIX awk (mawk here) converts and merges the ranges.
# Versions come from the pinned build args, not from LibreOfficeKit. fontconfig runs here, at
# build time only; the supervisor never runs it. A derived font image runs this again (as root)
# after /opt/convertd/cache-fonts.sh.
set -euo pipefail
: "${LO_VERSION:?}" "${PDFIUM_BUILD:?}"
out="${BUILD_INFO_PATH:-/opt/convertd/build-info.json}"
probe="${FONT_PROBE_LIST:-/opt/convertd/probe-families.txt}"

json_str() { local s="${1//\\/\\\\}"; s="${s//\"/\\\"}"; printf '"%s"' "${s}"; }

# One line per family entry; fontconfig lists localized names comma-separated, and 001 splits
# them at start-up, so the raw entries are kept.
fonts="$(fc-list --format '%{family}\n' | sed '/^$/d' | LC_ALL=C sort -u \
         | while IFS= read -r f; do json_str "${f}"; printf ','; done)"
fonts="[${fonts%,}]"
default="$(fc-match --format '%{family[0]}' sans-serif)"
[[ -n "${default}" ]] || { echo "fc-match found no sans-serif" >&2; exit 1; }

subs=""
while IFS= read -r fam || [[ -n "${fam}" ]]; do
  fam="${fam%%#*}"; fam="$(printf '%s' "${fam}" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
  [[ -n "${fam}" ]] || continue
  got="$(fc-match --format '%{family[0]}' "${fam}")"
  [[ -n "${got}" ]] || { echo "fc-match returned nothing for ${fam}" >&2; exit 1; }
  subs+="$(json_str "${fam}"):$(json_str "${got}"),"
done < "${probe}"
subs="{${subs%,}}"

# fc-list prints each face's charset as hex ranges ("20-7e a0-17f 192"); one token per line,
# converted to decimal pairs, sorted, then merged where they touch or overlap.
coverage="$(fc-list --format '%{charset}\n' | tr ' ' '\n' | sed '/^$/d' | awk '
  function hex(s,   i, c, n) {
    n = 0; s = tolower(s)
    for (i = 1; i <= length(s); i++) {
      c = index("0123456789abcdef", substr(s, i, 1)) - 1
      if (c < 0) { print "write-build-info: bad charset token " s > "/dev/stderr"; exit 2 }
      n = n * 16 + c
    }
    return n
  }
  { n = split($0, p, "-"); a = hex(p[1]); b = (n > 1) ? hex(p[2]) : a; print a, b }' \
  | LC_ALL=C sort -n -k1,1 -k2,2 | awk '
  NR == 1 { s = $1; e = $2; next }
  { if ($1 <= e + 1) { if ($2 > e) e = $2 } else { printf "[%d,%d],", s, e; s = $1; e = $2 } }
  END { if (NR) printf "[%d,%d]", s, e }')"
[[ -n "${coverage}" ]] || { echo "fc-list reported no charset" >&2; exit 1; }
coverage="[${coverage}]"

mkdir -p "$(dirname "${out}")"
printf '{"libreoffice":%s,"pdfium":%s,"fonts":%s,"font_substitutes":%s,"default_substitute":%s,"coverage":%s}\n' \
  "$(json_str "${LO_VERSION}")" "$(json_str "${PDFIUM_BUILD}")" "${fonts}" "${subs}" \
  "$(json_str "${default}")" "${coverage}" \
  > "${out}"
chmod 0644 "${out}"
