# Spike B image: pinned PDFium from bblanchon/pdfium-binaries, plus the spike binary.
FROM rust:1.99-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked -j 4

FROM debian:trixie-slim
ARG PDFIUM_VERSION=8179
ARG PDFIUM_ARCH=arm64
ARG PDFIUM_SHA256=18bb6d568257b28d14f6c3b8bfb48caf73d4615b319ca4d43b424c2a03431d9f
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -fsSL -o /tmp/pdfium.tgz "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${PDFIUM_VERSION}/pdfium-linux-${PDFIUM_ARCH}.tgz" \
    && echo "${PDFIUM_SHA256}  /tmp/pdfium.tgz" | sha256sum -c - \
    && mkdir -p /opt/pdfium && tar -xzf /tmp/pdfium.tgz -C /opt/pdfium \
    && rm /tmp/pdfium.tgz && apt-get purge -y curl && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/spike-pdfium /usr/local/bin/spike-pdfium
RUN useradd -m convert
USER convert
