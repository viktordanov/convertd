#!/usr/bin/env bash
# docker/install-engines.sh.
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

# fonts-wqy-microhei: the CJK fallback (Han, kana, Hangul), one 5 MB file, so that CJK text
# never renders blank. FONTS_CJK=1 adds Noto CJK, with the right glyph forms per region (91 MB).
fonts=(fonts-liberation fonts-crosextra-carlito fonts-crosextra-caladea fonts-dejavu-core
       fonts-noto-core fonts-noto-color-emoji fonts-wqy-microhei)
[[ "${FONTS_CJK:-0}" == 1 ]] && fonts+=(fonts-noto-cjk)
extra=()
# Dev only: what the engine tests use beyond the product.
[[ "${mode}" == dev ]] && extra+=(locales procps)

apt-get update
# Shared libraries that LibreOffice needs beyond debian:trixie-slim. For libglib2.0-0, trixie's
# real package is libglib2.0-0t64 (the 64-bit time_t rename), which provides the old name.
apt-get install -y --no-install-recommends \
  fontconfig libxinerama1 libdbus-1-3 libcairo2 libnss3 libxslt1.1 libcups2 libglib2.0-0t64 \
  libsm6 libice6 libxrender1 libx11-xcb1 \
  "${fonts[@]}" ${extra[@]+"${extra[@]}"} /debs/*.deb

lo_dir="libreoffice${LO_VERSION%.*}"
test -f "/opt/${lo_dir}/program/libmergedlo.so"
ln -s "${lo_dir}" /opt/libreoffice
# 10-convertd-libreoffice-fonts.conf points fontconfig at LibreOffice's font directories, so
# ooofonts and OpenSymbol are in the system cache and in build-info.json. LibreOffice registers
# them by their real path, and fontconfig keys the cache on the path as written, so the conf
# must name the real directory: otherwise every worker writes a user cache at start.
fc_conf=/etc/fonts/conf.d/10-convertd-libreoffice-fonts.conf
sed -i "s|<dir>/opt/libreoffice/|<dir>/opt/${lo_dir}/|" "${fc_conf}"
for d in share/fonts/truetype program/resource/common/fonts; do
  test -d "/opt/${lo_dir}/${d}"
  grep -qxF "  <dir>/opt/${lo_dir}/${d}</dir>" "${fc_conf}"
done

# Install-level configuration (macro lock-down etc.), if any.
shopt -s nullglob
for f in /lo-config/*.xcd; do install -m 0644 "${f}" /opt/libreoffice/share/registry/; done
shopt -u nullglob
# Optional read-only profile template, read through LO_PROFILE_TEMPLATE.
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
# The dpkg excludes must not have dropped a family that docker/README.md promises.
for fam in "Liberation Sans" "Liberation Serif" "Liberation Mono" Carlito Caladea "DejaVu Sans" \
           "Noto Sans" "Noto Serif" "Noto Sans Mono" "Noto Sans Symbols" "Noto Sans Symbols2" \
           "Noto Sans Math" "Noto Color Emoji" OpenSymbol "Noto Sans Thai" "Noto Sans Ethiopic" \
           "WenQuanYi Micro Hei"; do
  fc-list ":family=${fam}" family | grep . >/dev/null || { echo "font family missing: ${fam}" >&2; exit 1; }
done
# Coverage gate: some installed font must have each of these letters, or text in that script
# renders blank. One letter per script: Devanagari, Thai, Ethiopic,
# Han, hiragana, katakana, Hangul, Arabic, Hebrew, then Bengali, Gujarati, Gurmukhi, Kannada,
# Malayalam, Oriya, Tamil, Telugu, Sinhala, Khmer, Myanmar, Tibetan, Thaana, Syriac,
# Mongolian, Cherokee, Yi and Ol Chiki.
for cp in 0939 0E01 12A0 4E2D 306E 30AB D55C 0639 05D0 \
          09AC 0A95 0A15 0C95 0D15 0B15 0BA4 0C24 0D9A 1797 1019 0F40 0780 0710 \
          1820 13A0 A000 1C5A; do
  fc-list ":charset=${cp}" family | grep . >/dev/null || { echo "no font has U+${cp}" >&2; exit 1; }
done

apt-get clean
rm -rf /var/lib/apt/lists/* /var/cache/apt/* /var/log/* /var/cache/ldconfig/aux-cache \
       /var/cache/debconf/*-old /var/lib/dpkg/*-old

# No setuid or setgid file: the service never changes its user, so these
# bits only help an attacker who runs code in the container. The base image ships them on
# passwd, su, mount and their kin. -xdev keeps the bind mounts of this build out of the walk.
# The build log lists what was stripped; docker/smoke.sh fails if any bit is back.
find / -xdev -type f -perm /6000 -print -exec chmod a-s {} +
left="$(find / -xdev -type f -perm /6000 -print)"
[[ -z "${left}" ]] || { echo "setuid or setgid files remain: ${left}" >&2; exit 1; }

groupadd --system --gid 10001 convertd
if [[ "${mode}" == runtime ]]; then
  useradd --system --uid 10001 --gid 10001 --home-dir /tmp --no-create-home \
          --shell /usr/sbin/nologin convertd
else
  # Dev: a real home for CARGO_HOME and the registry cache.
  useradd --uid 10001 --gid 10001 --create-home --home-dir /home/convertd --shell /bin/bash convertd
fi
