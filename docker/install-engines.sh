#!/usr/bin/env bash
# docker/install-engines.sh (ledger item 003).
# The one place that installs LibreOffice, its libraries and the fonts. The Dockerfile runs it
# in two stages, so the product image and the dev/test image cannot drift apart:
#   runtime-base  (debian:trixie-slim)  install-engines.sh runtime
#   dev           (rust:1.99-trixie)    install-engines.sh dev
# It expects, as bind mounts: /debs (the allow-listed .debs from the engines stage) and
# /lo-config (docker/libreoffice: *.xcd, ldd-allow.txt, optional profile/).
# Build args arrive as environment: LO_VERSION, FONTS_CJK.
# The shared-library check is not here: lib-check runs it natively with readelf.
set -euo pipefail
mode="${1:?runtime|dev}"

fonts=(fonts-liberation fonts-crosextra-carlito fonts-crosextra-caladea fonts-dejavu-core
       fonts-noto-core fonts-noto-color-emoji)
[[ "${FONTS_CJK:-0}" == 1 ]] && fonts+=(fonts-noto-cjk)
extra=()
# Dev only: what lane 002's tests use beyond the product (002-engine-workers/dev.Dockerfile.txt:25,28).
[[ "${mode}" == dev ]] && extra+=(locales procps)

apt-get update
# Shared libraries that the M0 spike image needed (docs/m0-spikes.md). The spike named
# libglib2.0-0; trixie's real package is libglib2.0-0t64 (the 64-bit time_t rename), which
# provides the old name.
apt-get install -y --no-install-recommends \
  fontconfig libxinerama1 libdbus-1-3 libcairo2 libnss3 libxslt1.1 libcups2 libglib2.0-0t64 \
  libsm6 libice6 libxrender1 libx11-xcb1 \
  "${fonts[@]}" ${extra[@]+"${extra[@]}"} /debs/*.deb

lo_dir="libreoffice${LO_VERSION%.*}"
test -f "/opt/${lo_dir}/program/libmergedlo.so"
ln -s "${lo_dir}" /opt/libreoffice
# 10-rs-convert-libreoffice-fonts.conf points fontconfig at LibreOffice's font directories, so
# ooofonts and OpenSymbol are in the system cache and in build-info.json. LibreOffice registers
# them by their real path, and fontconfig keys the cache on the path as written, so the conf
# must name the real directory: otherwise every worker writes a user cache at start.
fc_conf=/etc/fonts/conf.d/10-rs-convert-libreoffice-fonts.conf
sed -i "s|<dir>/opt/libreoffice/|<dir>/opt/${lo_dir}/|" "${fc_conf}"
for d in share/fonts/truetype program/resource/common/fonts; do
  test -d "/opt/${lo_dir}/${d}"
  grep -qxF "  <dir>/opt/${lo_dir}/${d}</dir>" "${fc_conf}"
done

# Install-level configuration from lane 002 (macro lock-down etc.), if any.
shopt -s nullglob
for f in /lo-config/*.xcd; do install -m 0644 "${f}" /opt/libreoffice/share/registry/; done
shopt -u nullglob
# Optional read-only profile template (lane 002), read through LO_PROFILE_TEMPLATE.
if [[ -d /lo-config/profile ]]; then
  mkdir -p /opt/convertd
  cp -a /lo-config/profile /opt/convertd/lo-profile
  chmod -R a+rX,go-w /opt/convertd/lo-profile
fi

if [[ "${mode}" == runtime ]] && { command -v java || command -v python3; }; then
  echo "java or python in the product image" >&2; exit 1
fi

if [[ "${mode}" == dev ]]; then
  sed -i 's/^# en_GB.UTF-8/en_GB.UTF-8/; s/^# en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen
  locale-gen
fi

# The system font cache, with whole-second directory mtimes so that it survives any image
# export (docker/cache-fonts.sh says why). The Dockerfile copies the script in first.
bash /opt/convertd/cache-fonts.sh
# The dpkg excludes must not have dropped a family PLAN.md promises.
for fam in "Liberation Sans" "Liberation Serif" "Liberation Mono" Carlito Caladea "DejaVu Sans" \
           "Noto Sans" "Noto Serif" "Noto Sans Mono" "Noto Sans Symbols" "Noto Sans Symbols2" \
           "Noto Sans Math" "Noto Color Emoji" OpenSymbol; do
  fc-list ":family=${fam}" family | grep . >/dev/null || { echo "font family missing: ${fam}" >&2; exit 1; }
done

apt-get clean
rm -rf /var/lib/apt/lists/* /var/cache/apt/* /var/log/* /var/cache/ldconfig/aux-cache \
       /var/cache/debconf/*-old /var/lib/dpkg/*-old

groupadd --system --gid 10001 convertd
if [[ "${mode}" == runtime ]]; then
  useradd --system --uid 10001 --gid 10001 --home-dir /tmp --no-create-home \
          --shell /usr/sbin/nologin convertd
else
  # Dev: a real home for CARGO_HOME and the registry cache.
  useradd --uid 10001 --gid 10001 --create-home --home-dir /home/convertd --shell /bin/bash convertd
fi
