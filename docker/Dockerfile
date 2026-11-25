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

# LibreOffice from TDF. The SHA-256 values equal the ones TDF's mirror page publishes, and
# both tarballs carry a good signature from TDF's code signing key
# C2839ECAD9408FBE9531C3E9F434A1EFAFEEAEA3 (checked when pinned).
ARG LO_VERSION=26.8.3
# Four-part build number (the tarball's top directory): the archive URL and build-info.json.
ARG LO_BUILD=26.8.3.1
ARG LO_SHA256_ARM64=378a0766e4a815a5a96fb7473f346d072e23e3cc28606bdc8cd6f834cfaa14db
ARG LO_SHA256_AMD64=e7cd3538e85818ff9dd9bb65386769fde927011242aace5a144989f4000dcabf

# PDFium from bblanchon/pdfium-binaries; the SHA-256 values equal GitHub's asset digests.
ARG PDFIUM_BUILD=8179
ARG PDFIUM_SHA256_ARM64=18bb6d568257b28d14f6c3b8bfb48caf73d4615b319ca4d43b424c2a03431d9f
ARG PDFIUM_SHA256_AMD64=b4766a6bf2856efbcc6c3cc7d6ae9022bdc6bb36ddc1acdd92d1f1873f773f4b

# ---- Engine downloads: built on the build platform, never in the runtime ------------------
# Each download lands in a cache mount under its SHA-256 and is checked before use. TDF removes
# a point release from /stable/ when the next one ships; the archive keeps it under the
# four-part build number, which is the fallback.
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
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,id=engine-downloads,target=/downloads,sharing=locked \
    case "${TARGETARCH}" in \
      amd64) lo_dir=x86_64;  lo_file=x86-64;  lo_sha="${LO_SHA256_AMD64}"; pd_arch=x64;   pd_sha="${PDFIUM_SHA256_AMD64}" ;; \
      arm64) lo_dir=aarch64; lo_file=aarch64; lo_sha="${LO_SHA256_ARM64}"; pd_arch=arm64; pd_sha="${PDFIUM_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    [[ -n "${lo_sha}" ]] || { echo "LO_SHA256_${TARGETARCH^^} is not pinned" >&2; exit 1; }; \
    [[ -n "${pd_sha}" ]] || { echo "PDFIUM_SHA256_${TARGETARCH^^} is not pinned" >&2; exit 1; }; \
    lo_tgz="/downloads/lo-${lo_sha}.tar.gz"; \
    if ! echo "${lo_sha}  ${lo_tgz}" | sha256sum -c --status - 2>/dev/null; then \
      curl -fsSL -o "${lo_tgz}" \
        "https://download.documentfoundation.org/libreoffice/stable/${LO_VERSION}/deb/${lo_dir}/LibreOffice_${LO_VERSION}_Linux_${lo_file}_deb.tar.gz" \
      || curl -fsSL -o "${lo_tgz}" \
        "https://downloadarchive.documentfoundation.org/libreoffice/old/${LO_BUILD}/deb/${lo_dir}/LibreOffice_${LO_BUILD}_Linux_${lo_file}_deb.tar.gz"; \
    fi; \
    echo "${lo_sha}  ${lo_tgz}" | sha256sum -c -; \
    pd_tgz="/downloads/pdfium-${pd_sha}.tgz"; \
    if ! echo "${pd_sha}  ${pd_tgz}" | sha256sum -c --status - 2>/dev/null; then \
      curl -fsSL -o "${pd_tgz}" "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${PDFIUM_BUILD}/pdfium-linux-${pd_arch}.tgz"; \
    fi; \
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
