# syntax=docker/dockerfile:1.12
# One image: convertd + pinned LibreOffice (TDF .deb allow-list) + pinned PDFium + fonts.
# Context: the repository root.   File: docker/Dockerfile.   Driver: build_docker.sh.
#
# Targets:
#   runtime        (default) the service image
#   runtime-base   engines, fonts, build-info and user, no convertd
#   dev            Rust toolchain + the same engines, for lane 002's engine tests (host arch only)
#   engine-check   runtime-base + the spike binaries (plan steps 2-4 only, then deleted)
#
# The Rust stages run on $BUILDPLATFORM and cross-compile to $TARGETARCH, so no rustc runs
# under QEMU. The library check (lib-check) also runs natively, with readelf, because ldd is
# unreliable under emulation. Only the apt install and fontconfig run as the target arch.

# ---- Pins. Each one is checked; an empty hash fails the build. ---------------------------
# Base images by index digest (docker buildx imagetools inspect, 2026-11-25).
ARG RUST_IMAGE=rust:1.99-trixie@sha256:8a4e90c7946b74626488b1446f712c0fe0ed1a596cbd3d871684bc9c5433ae07
ARG RUNTIME_BASE=debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0
ARG RUST_VERSION=1.99.0
ARG CARGO_CHEF_VERSION=0.1.78

# LibreOffice from TDF. Each SHA-256 equals TDF's .sha256 file next to the tarball (HTTPS,
# download.documentfoundation.org) and the hash of our own download. Both tarballs carry a
# good signature from key C2839ECAD9408FBE9531C3E9F434A1EFAFEEAEA3, the only key that
# keyserver.ubuntu.com lists for build@documentfoundation.org (checked when pinned). That
# fingerprint is not confirmed against a TDF page: none that could be read shows it.
ARG LO_VERSION=26.8.3
# Four-part build number (the tarball's top directory): the archive URL and build-info.json.
ARG LO_BUILD=26.8.3.1
ARG LO_SHA256_ARM64=378a0766e4a815a5a96fb7473f346d072e23e3cc28606bdc8cd6f834cfaa14db
ARG LO_SHA256_AMD64=e7cd3538e85818ff9dd9bb65386769fde927011242aace5a144989f4000dcabf

# PDFium from bblanchon/pdfium-binaries; the SHA-256 values equal GitHub's asset digests.
ARG PDFIUM_BUILD=8179
ARG PDFIUM_SHA256_ARM64=18bb6d568257b28d14f6c3b8bfb48caf73d4615b319ca4d43b424c2a03431d9f
ARG PDFIUM_SHA256_AMD64=b4766a6bf2856efbcc6c3cc7d6ae9022bdc6bb36ddc1acdd92d1f1873f773f4b

# ---- cargo-chef, built once on the build platform ------------------------------------------
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS chef-tool
ARG CARGO_CHEF_VERSION
ARG CARGO_JOBS=2
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    test -n "${CARGO_CHEF_VERSION}" && \
    cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}" -j "${CARGO_JOBS}"

# ---- Rust cross toolchain for $TARGETARCH --------------------------------------------------
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS rust-cross
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ARG RUST_VERSION
ARG TARGETARCH
ARG BUILDARCH
# The Docker VM has about 4 GB, and buildx builds both platforms at once. build_docker.sh
# passes 2 for a two-platform build and 4 for one.
ARG CARGO_JOBS=2
# RUSTUP_TOOLCHAIN overrides rust-toolchain.toml, so rustup does not fetch rustfmt/clippy here.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_BUILD_JOBS=${CARGO_JOBS} \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
RUN case "${TARGETARCH}" in \
      amd64) triple=x86_64-unknown-linux-gnu;  cross="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) triple=aarch64-unknown-linux-gnu; cross="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    echo "${triple}" > /etc/rust-target; \
    if [[ "${TARGETARCH}" != "${BUILDARCH}" ]]; then \
      apt-get update && apt-get install -y --no-install-recommends ${cross} && rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add --toolchain "${RUST_VERSION}" "${triple}"
WORKDIR /src

# ---- Dependency recipe ----------------------------------------------------------------------
# Cargo.toml declares no [[test]], [[example]] or [[bench]] target and no build.rs, so the
# manifest, the lock file and src/ are all that `cargo build --bin convertd` reads. tests/ and
# examples/ stay out, so a test change does not rebuild convertd. If the manifest starts to
# declare such a target, copy its folder here and in the builder, or cargo rejects the
# manifest. spikes/ and tools/ are separate crates and never enter the recipe.
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS planner
COPY --from=chef-tool /usr/local/cargo/bin/cargo-chef /usr/local/cargo/bin/cargo-chef
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ---- convertd -------------------------------------------------------------------------------
# Cross-compiled on the build platform. cook builds the dependencies from the recipe alone, so
# that layer stays cached until Cargo.toml or Cargo.lock changes.
FROM rust-cross AS builder
COPY --from=chef-tool /usr/local/cargo/bin/cargo-chef /usr/local/cargo/bin/cargo-chef
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    cargo chef cook --release --locked --target "$(cat /etc/rust-target)" --bin convertd \
      --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The commit reaches the code through option_env!("CONVERTD_COMMIT") (src/rest/state.rs).
# Declared after cook, so a new commit rebuilds only the crate. Unset when empty, so the code
# sees None and not "".
ARG CONVERTD_COMMIT
# The real sources replace cook's skeleton; touch them so cargo never keeps a skeleton build.
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    if [[ -z "${CONVERTD_COMMIT:-}" ]]; then unset CONVERTD_COMMIT; fi; \
    find src -name '*.rs' -exec touch {} +; \
    triple="$(cat /etc/rust-target)"; \
    cargo build --release --locked --target "${triple}" --bin convertd; \
    install -D -m 0755 "target/${triple}/release/convertd" /out/convertd

# ---- Spike binaries, only for the engine-check target --------------------------------------
FROM rust-cross AS spike-build
COPY spikes/lok ./spikes/lok
COPY spikes/pdfium ./spikes/pdfium
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    triple="$(cat /etc/rust-target)"; \
    for s in lok pdfium; do \
      cargo build --release --locked --target "${triple}" --manifest-path "spikes/${s}/Cargo.toml"; \
      install -D -m 0755 "spikes/${s}/target/${triple}/release/spike-${s}" "/out/spike-${s}"; \
    done

# ---- Engine downloads: built on the build platform, never in the runtime ------------------
# Each download lands in a cache mount under its SHA-256. docker/fetch-verified.sh tries the
# URLs in order and keeps the first one that gives the pinned SHA-256.
# LibreOffice comes from LO_STABLE_MIRRORS, then from the TDF archive. The first entry is TDF's
# redirector, which picks a mirror near the client. On 2026-11-26 the redirector sent a GitHub
# amd64 runner to a mirror whose chain Debian's curl rejects (error 60), so fixed mirrors in
# other countries follow it. TDF removes a point release from /stable/ when the next one ships;
# the archive keeps it under the four-part build number. The archive URL is UNVERIFIED for an
# archived release: 26.8.3 is still current, and the archive host redirects to the mirrors too.
FROM --platform=$BUILDPLATFORM ${RUNTIME_BASE} AS engines
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ARG TARGETARCH
ARG LO_VERSION
ARG LO_BUILD
ARG LO_SHA256_ARM64
ARG LO_SHA256_AMD64
ARG PDFIUM_BUILD
ARG PDFIUM_SHA256_ARM64
ARG PDFIUM_SHA256_AMD64
# Base URLs that hold libreoffice/stable/<version>/deb/<dir>/, tried in this order. The fixed
# mirrors are in Germany, the United States and Denmark (TDF's .mirrorlist gives the paths).
ARG LO_STABLE_MIRRORS="https://download.documentfoundation.org/libreoffice/stable https://ftp.fau.de/tdf/libreoffice/stable https://ftp.osuosl.org/pub/tdf/libreoffice/stable https://mirrors.dotsrc.org/tdf/libreoffice/stable"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,id=engine-downloads,target=/downloads,sharing=locked \
    --mount=type=bind,source=docker/fetch-verified.sh,target=/fetch-verified.sh \
    case "${TARGETARCH}" in \
      amd64) lo_dir=x86_64;  lo_file=x86-64;  lo_sha="${LO_SHA256_AMD64}"; pd_arch=x64;   pd_sha="${PDFIUM_SHA256_AMD64}" ;; \
      arm64) lo_dir=aarch64; lo_file=aarch64; lo_sha="${LO_SHA256_ARM64}"; pd_arch=arm64; pd_sha="${PDFIUM_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    [[ -n "${lo_sha}" ]] || { echo "LO_SHA256_${TARGETARCH^^} is not pinned" >&2; exit 1; }; \
    [[ -n "${pd_sha}" ]] || { echo "PDFIUM_SHA256_${TARGETARCH^^} is not pinned" >&2; exit 1; }; \
    lo_tgz="/downloads/lo-${lo_sha}.tar.gz"; \
    lo_urls=(); \
    for m in ${LO_STABLE_MIRRORS}; do \
      lo_urls+=("${m}/${LO_VERSION}/deb/${lo_dir}/LibreOffice_${LO_VERSION}_Linux_${lo_file}_deb.tar.gz"); \
    done; \
    lo_urls+=("https://downloadarchive.documentfoundation.org/libreoffice/old/${LO_BUILD}/deb/${lo_dir}/LibreOffice_${LO_BUILD}_Linux_${lo_file}_deb.tar.gz"); \
    bash /fetch-verified.sh "${lo_sha}" "${lo_tgz}" "${lo_urls[@]}"; \
    echo "${lo_sha}  ${lo_tgz}" | sha256sum -c -; \
    pd_tgz="/downloads/pdfium-${pd_sha}.tgz"; \
    bash /fetch-verified.sh "${pd_sha}" "${pd_tgz}" \
      "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${PDFIUM_BUILD}/pdfium-linux-${pd_arch}.tgz"; \
    echo "${pd_sha}  ${pd_tgz}" | sha256sum -c -; \
    mkdir -p /tmp/lo /tmp/pdfium /out/debs /out/pdfium/lib; \
    tar -xzf "${lo_tgz}" -C /tmp/lo --strip-components=1; \
    series="${LO_VERSION%.*}"; \
    for p in "libreoffice${series}-ure" "libreoffice${series}" "libobasis${series}-core" \
             "libobasis${series}-writer" "libobasis${series}-calc" "libobasis${series}-impress" \
             "libobasis${series}-draw" "libobasis${series}-graphicfilter" "libobasis${series}-images" \
             "libobasis${series}-en-us" "libreoffice${series}-en-us" \
             "libobasis${series}-libreofficekit-data" "libobasis${series}-ooofonts"; do \
      m=(/tmp/lo/DEBS/"${p}"_*.deb); \
      [[ ${#m[@]} -eq 1 && -f "${m[0]}" ]] || { echo "expected one ${p}_*.deb, got: ${m[*]}" >&2; exit 1; }; \
      cp "${m[0]}" /out/debs/; \
    done; \
    tar -xzf "${pd_tgz}" -C /tmp/pdfium; \
    install -m 0644 /tmp/pdfium/lib/libpdfium.so /out/pdfium/lib/libpdfium.so; \
    install -m 0644 /tmp/pdfium/LICENSE /out/pdfium/LICENSE; \
    cp -r /tmp/pdfium/licenses /out/pdfium/licenses; \
    chmod -R a+rX,go-w /out/pdfium; \
    rm -rf /tmp/lo /tmp/pdfium

# ---- Runtime base: Debian userland, LibreOffice, PDFium, fonts, build-info, user -----------
FROM ${RUNTIME_BASE} AS runtime-base
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ARG LO_VERSION
ARG LO_BUILD
ARG PDFIUM_BUILD
# 1 adds Noto CJK (size unmeasured).
ARG FONTS_CJK=0
# What dpkg never unpacks. docs: documentation, man pages, message catalogs (product only).
# fonts: the Noto subset. libreoffice: the LibreOffice parts headless never loads. Both are in
# product and dev, so both images see the same families and the same LibreOffice.
COPY docker/dpkg/docs.cfg /etc/dpkg/dpkg.cfg.d/90-rs-convert-docs
COPY docker/dpkg/fonts.cfg /etc/dpkg/dpkg.cfg.d/91-rs-convert-fonts
COPY docker/dpkg/libreoffice.cfg /etc/dpkg/dpkg.cfg.d/92-rs-convert-libreoffice
# fonts.json: {family -> metric_compatible} for FONT_SUBSTITUTED (005 §5). The aliases add the
# Office families Debian does not map; build-info.json records what fc-match then picks.
COPY docker/fonts/fonts.json /etc/convertd/fonts.json
COPY docker/fonts/10-libreoffice-fonts.conf /etc/fonts/conf.d/10-rs-convert-libreoffice-fonts.conf
COPY docker/fonts/35-rs-convert-aliases.conf /etc/fonts/conf.d/35-rs-convert-aliases.conf
# Builds the font cache so it stays valid after a tar export; kept for derived font images.
COPY docker/cache-fonts.sh /opt/convertd/cache-fonts.sh
# docker/install-engines.sh is shared with the dev target, so the two cannot drift. The script
# and the .debs are bind mounts, so neither becomes a layer.
RUN --mount=type=bind,from=engines,source=/out/debs,target=/debs \
    --mount=type=bind,source=docker/libreoffice,target=/lo-config \
    --mount=type=bind,source=docker/install-engines.sh,target=/install-engines.sh \
    LO_VERSION="${LO_VERSION}" FONTS_CJK="${FONTS_CJK}" bash /install-engines.sh runtime
COPY --from=engines /out/pdfium /opt/pdfium
# Writes /opt/convertd/build-info.json (versions from the pinned args, fonts from fc-list,
# font_substitutes from fc-match per family in probe-families.txt, and the default substitute).
# The script stays in the image so a derived font image can run it again.
COPY docker/write-build-info.sh /opt/convertd/write-build-info.sh
COPY docker/fonts/probe-families.txt /opt/convertd/probe-families.txt
RUN LO_VERSION="${LO_BUILD:-${LO_VERSION}}" PDFIUM_BUILD="${PDFIUM_BUILD}" \
    bash /opt/convertd/write-build-info.sh
# Names agreed with lane 002. PDFIUM_BUILD exists because PDFium has no version call.
ENV HOME=/tmp \
    LANG=C.UTF-8 \
    SAL_USE_VCLPLUGIN=svp \
    LO_PROGRAM_DIR=/opt/libreoffice/program \
    PDFIUM_LIB_DIR=/opt/pdfium/lib \
    PDFIUM_BUILD=${PDFIUM_BUILD} \
    TMP_DIR=/tmp/convert \
    PORT=8080 \
    LOG_MODE=prod
# Added in the same commit as docker/libreoffice/profile/, if lane 002 provides one:
# ENV LO_PROFILE_TEMPLATE=/opt/convertd/lo-profile
WORKDIR /tmp
USER 10001:10001

# ---- Native library check: readelf on the build platform, no emulation ---------------------
# Every DT_NEEDED of every LibreOffice and PDFium object must resolve to a file in the target
# rootfs, unless docker/libreoffice/ldd-allow.txt lists that exact "<object>: <library>" pair.
# runtime and engine-check copy a marker from here, so BuildKit always runs it.
FROM --platform=$BUILDPLATFORM ${RUNTIME_BASE} AS lib-check
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends binutils && rm -rf /var/lib/apt/lists/*
COPY --from=runtime-base /opt /rootfs/opt
COPY --from=runtime-base /usr/lib /rootfs/usr/lib
COPY --from=runtime-base /etc/ld.so.conf.d /rootfs/etc/ld.so.conf.d
COPY docker/libreoffice/ldd-allow.txt /allow.txt
RUN case "${TARGETARCH}" in amd64) tri=x86_64-linux-gnu ;; arm64) tri=aarch64-linux-gnu ;; esac; \
    dirs=("/rootfs/usr/lib/${tri}" /rootfs/usr/lib /rootfs/opt/libreoffice/program /rootfs/opt/pdfium/lib); \
    while read -r d; do dirs+=("/rootfs${d}"); done < <(cat /rootfs/etc/ld.so.conf.d/*.conf 2>/dev/null | grep '^/' || true); \
    grep -v -E '^[[:space:]]*(#|$)' /allow.txt > /allow.clean || true; \
    readelf -d /rootfs/opt/libreoffice/program/libmergedlo.so | grep 'NEEDED.*libc\.so\.6' >/dev/null \
      || { echo "positive control failed: readelf sees no libc.so.6 in libmergedlo.so" >&2; exit 1; }; \
    missing=""; n=0; allowed=0; \
    while IFS= read -r -d '' so; do \
      n=$((n + 1)); \
      while read -r lib; do \
        found=0; for d in "${dirs[@]}"; do [[ -e "${d}/${lib}" ]] && { found=1; break; }; done; \
        if [[ "${found}" == 0 ]]; then \
          if grep -qxF "${so#/rootfs}: ${lib}" /allow.clean; then allowed=$((allowed + 1)); \
          else missing+="${so#/rootfs}: ${lib}"$'\n'; fi; \
        fi; \
      done < <(readelf -d "${so}" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); \
    done < <(find /rootfs/opt/libreoffice/program /rootfs/opt/pdfium/lib -name '*.so*' -type f -print0); \
    if [[ -n "${missing}" ]]; then echo "unresolved libraries:" >&2; printf '%s' "${missing}" >&2; exit 1; fi; \
    echo "lib-check (${TARGETARCH}): ${n} objects, every DT_NEEDED resolved except ${allowed}" \
         "allow-listed pairs (docker/libreoffice/ldd-allow.txt)" | tee /lib-check.ok

# ---- Dev/test image for lane 002's engine tests (replaces 002's dev.Dockerfile.txt) -------
# Toolchain + the same engines, installed by the same script. Build for the host only:
#   docker buildx build -f docker/Dockerfile --target dev --load -t rs-convert-dev .
# Run with the source mounted and a job-local target dir (AGENT-RULES.md rule 5):
#   docker run --rm -v "$PWD":/src -v <scratch>/target:/target \
#     -e CARGO_TARGET_DIR=/target rs-convert-dev cargo test -- --include-ignored
FROM ${RUST_IMAGE} AS dev
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ARG LO_VERSION
ARG LO_BUILD
ARG PDFIUM_BUILD
ARG RUST_VERSION
ARG FONTS_CJK=0
# The font and LibreOffice excludes only: the docs/locale excludes would fight locale-gen here.
COPY docker/dpkg/fonts.cfg /etc/dpkg/dpkg.cfg.d/91-rs-convert-fonts
COPY docker/dpkg/libreoffice.cfg /etc/dpkg/dpkg.cfg.d/92-rs-convert-libreoffice
COPY docker/fonts/fonts.json /etc/convertd/fonts.json
COPY docker/fonts/10-libreoffice-fonts.conf /etc/fonts/conf.d/10-rs-convert-libreoffice-fonts.conf
COPY docker/fonts/35-rs-convert-aliases.conf /etc/fonts/conf.d/35-rs-convert-aliases.conf
# Builds the font cache so it stays valid after a tar export; kept for derived font images.
COPY docker/cache-fonts.sh /opt/convertd/cache-fonts.sh
RUN --mount=type=bind,from=engines,source=/out/debs,target=/debs \
    --mount=type=bind,source=docker/libreoffice,target=/lo-config \
    --mount=type=bind,source=docker/install-engines.sh,target=/install-engines.sh \
    LO_VERSION="${LO_VERSION}" FONTS_CJK="${FONTS_CJK}" bash /install-engines.sh dev; \
    rustup component add --toolchain "${RUST_VERSION}" rustfmt clippy
COPY --from=engines /out/pdfium /opt/pdfium
COPY docker/write-build-info.sh /opt/convertd/write-build-info.sh
COPY docker/fonts/probe-families.txt /opt/convertd/probe-families.txt
RUN LO_VERSION="${LO_BUILD:-${LO_VERSION}}" PDFIUM_BUILD="${PDFIUM_BUILD}" \
    bash /opt/convertd/write-build-info.sh
ENV HOME=/home/convertd \
    LANG=C.UTF-8 \
    SAL_USE_VCLPLUGIN=svp \
    LO_PROGRAM_DIR=/opt/libreoffice/program \
    PDFIUM_LIB_DIR=/opt/pdfium/lib \
    PDFIUM_BUILD=${PDFIUM_BUILD} \
    CARGO_HOME=/home/convertd/.cargo
USER 10001:10001
WORKDIR /src

# ---- Engine check (plan steps 2-4 only) -----------------------------------------------------
FROM runtime-base AS engine-check
COPY --from=lib-check /lib-check.ok /opt/convertd/lib-check.ok
COPY --from=spike-build /out/ /usr/local/bin/

# ---- The service image -----------------------------------------------------------------------
FROM runtime-base AS runtime
ARG LO_BUILD
ARG PDFIUM_BUILD
ARG CONVERTD_VERSION=0.0.0-dev
ARG CONVERTD_COMMIT=unknown
ARG CONVERTD_BUILD_DATE=unknown
LABEL org.opencontainers.image.title="rs-convert" \
      org.opencontainers.image.description="convertd: office files, PDFs and images to PDF and page images" \
      org.opencontainers.image.source="https://github.com/viktordanov/rs-convert" \
      org.opencontainers.image.version="${CONVERTD_VERSION}" \
      org.opencontainers.image.revision="${CONVERTD_COMMIT}" \
      org.opencontainers.image.created="${CONVERTD_BUILD_DATE}" \
      rs-convert.libreoffice.version="${LO_BUILD}" \
      rs-convert.pdfium.version="chromium/${PDFIUM_BUILD}"
# The licence label is added when item 007 picks one.
# No init process: convertd is PID 1 and also sets itself as child subreaper (lane 002).
COPY --from=lib-check /lib-check.ok /opt/convertd/lib-check.ok
COPY --from=builder /out/convertd /usr/local/bin/convertd
EXPOSE 8080
# Docker and compose only; Kubernetes and Container Apps use their own HTTP probes.
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/convertd", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/convertd"]
CMD ["serve"]
