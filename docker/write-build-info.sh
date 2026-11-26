#!/usr/bin/env bash
# docker/write-build-info.sh, installed as /opt/convertd/write-build-info.sh.
# Writes /opt/convertd/build-info.json, the one source of engine versions and installed fonts
# for /v1/capabilities, the fingerprint and FONT_SUBSTITUTED (plans 001 and 005):
#   {"libreoffice": "26.8.3.1", "pdfium": "8179", "fonts": [...],
#    "font_substitutes": {"Calibri": "Carlito", ...}, "default_substitute": "..."}
# "font_substitutes" maps each family in the probe list (lane 005 owns it) to what fc-match picks
# in this image, aliases included; "default_substitute" is the fallback for any other name.
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

mkdir -p "$(dirname "${out}")"
printf '{"libreoffice":%s,"pdfium":%s,"fonts":%s,"font_substitutes":%s,"default_substitute":%s}\n' \
  "$(json_str "${LO_VERSION}")" "$(json_str "${PDFIUM_BUILD}")" "${fonts}" "${subs}" \
  "$(json_str "${default}")" \
  > "${out}"
chmod 0644 "${out}"
