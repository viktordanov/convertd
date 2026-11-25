# Spike A image: LibreOffice 26.8 from The Document Foundation, plus the spike binary.
# Build: docker build -t rs-convert-spike-lok .
FROM rust:1.99-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked -j 4

FROM debian:trixie-slim
ARG LO_VERSION=26.8.3
ARG LO_ARCH=aarch64
ARG LO_SHA256=378a0766e4a815a5a96fb7473f346d072e23e3cc28606bdc8cd6f834cfaa14db
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl procps \
        libxinerama1 libdbus-1-3 libcairo2 libnss3 libxslt1.1 libcups2 libglib2.0-0 libsm6 libice6 libxrender1 libx11-xcb1 \
        fontconfig fonts-liberation fonts-crosextra-carlito fonts-crosextra-caladea fonts-dejavu-core \
    && curl -fsSL -o /tmp/lo.tgz "https://download.documentfoundation.org/libreoffice/stable/${LO_VERSION}/deb/${LO_ARCH}/LibreOffice_${LO_VERSION}_Linux_${LO_ARCH}_deb.tar.gz" \
    && echo "${LO_SHA256}  /tmp/lo.tgz" | sha256sum -c - \
    && mkdir /tmp/lo && tar -xzf /tmp/lo.tgz -C /tmp/lo --strip-components=1 \
    && cd /tmp/lo/DEBS \
    && apt-get install -y --no-install-recommends \
        ./libreoffice26.8-ure_*.deb ./libreoffice26.8_*.deb ./libobasis26.8-core_*.deb \
        ./libobasis26.8-writer_*.deb ./libobasis26.8-calc_*.deb ./libobasis26.8-impress_*.deb ./libobasis26.8-draw_*.deb \
        ./libobasis26.8-graphicfilter_*.deb ./libobasis26.8-images_*.deb ./libobasis26.8-en-us_*.deb ./libreoffice26.8-en-us_*.deb \
        ./libobasis26.8-libreofficekit-data_*.deb ./libobasis26.8-ooofonts_*.deb \
    && rm -rf /tmp/lo /tmp/lo.tgz /var/lib/apt/lists/* \
    && fc-cache -f
COPY --from=build /src/target/release/spike-lok /usr/local/bin/spike-lok
RUN useradd -m convert
USER convert
WORKDIR /home/convert
