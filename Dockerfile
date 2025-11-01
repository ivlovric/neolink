# Neolink Docker image build scripts
# Copyright (c) 2020 George Hilliard,
#                    Andrew King,
#                    Miroslav Šedivý
# SPDX-License-Identifier: AGPL-3.0-only

FROM docker.io/rust:slim-bookworm AS build
ARG TARGETPLATFORM

ENV DEBIAN_FRONTEND=noninteractive
WORKDIR /usr/local/src/neolink

COPY . /usr/local/src/neolink

# Build the main program or copy from artifact
#
# We prefer copying from artifact to reduce
# build time on the github runners
#
# Because of this though, during normal
# github runner ops we are not testing the
# docker to see if it will build from scratch
# so if it is failing please make a PR
#
# hadolint ignore=DL3008
RUN  echo "TARGETPLATFORM: ${TARGETPLATFORM}"; \
  if [ -f "${TARGETPLATFORM}/neolink" ]; then \
    echo "Restoring from artifact"; \
    mkdir -p /usr/local/src/neolink/target/release/; \
    cp "${TARGETPLATFORM}/neolink" "/usr/local/src/neolink/target/release/neolink"; \
  else \
    echo "Building from scratch with FFmpeg support"; \
    apt-get update && \
        apt-get upgrade -y && \
        apt-get install -y --no-install-recommends \
          build-essential \
          pkg-config \
          openssl \
          libssl-dev \
          ca-certificates \
          protobuf-compiler && \
        apt-get clean -y && rm -rf /var/lib/apt/lists/* && \
        cargo build --release --no-default-features; \
  fi

# Create the release container. Match the base OS used to build
FROM debian:bookworm-slim AS runtime
ARG TARGETPLATFORM
ARG REPO
ARG VERSION
ARG OWNER
ARG MEDIAMTX_VERSION=v1.15.3

LABEL description="An image for the neolink program which is a reolink camera to rtsp translator"
LABEL repository="$REPO"
LABEL version="$VERSION"
LABEL maintainer="$OWNER"

# Install runtime dependencies
# ffmpeg: Media transcoding and format conversion
# ca-certificates: SSL/TLS certificates for HTTPS
# openssl: SSL/TLS library
# hadolint ignore=DL3008,DL3009
RUN apt-get update && \
    apt-get upgrade -y && \
    # Install base utilities and certificates first
    apt-get install -y --no-install-recommends \
        openssl \
        dnsutils \
        iputils-ping \
        ca-certificates \
        wget \
        # FFmpeg for transcoding (much smaller than GStreamer)
        ffmpeg && \
    apt-get clean -y && rm -rf /var/lib/apt/lists/*

# Download and install MediaMTX
# hadolint ignore=DL3008
RUN set -ex; \
    ARCH="$(dpkg --print-architecture)"; \
    case "${ARCH}" in \
      amd64) MEDIAMTX_ARCH='amd64' ;; \
      arm64) MEDIAMTX_ARCH='arm64v8' ;; \
      armhf) MEDIAMTX_ARCH='armv7' ;; \
      *) echo "Unsupported architecture: ${ARCH}"; exit 1 ;; \
    esac; \
    wget -O /tmp/mediamtx.tar.gz \
      "https://github.com/bluenviron/mediamtx/releases/download/${MEDIAMTX_VERSION}/mediamtx_${MEDIAMTX_VERSION}_linux_${MEDIAMTX_ARCH}.tar.gz"; \
    tar -xzf /tmp/mediamtx.tar.gz -C /usr/local/bin/ mediamtx; \
    chmod +x /usr/local/bin/mediamtx; \
    rm /tmp/mediamtx.tar.gz; \
    /usr/local/bin/mediamtx --version

COPY --from=build \
  /usr/local/src/neolink/target/release/neolink \
  /usr/local/bin/neolink
COPY docker/entrypoint-ffmpeg.sh /entrypoint.sh
COPY docker/mediamtx.yml /etc/mediamtx/mediamtx.yml

RUN chmod +x "/usr/local/bin/neolink" && \
    chmod +x /entrypoint.sh && \
    "/usr/local/bin/neolink" --version && \
    mkdir -m 0700 /root/.config/

ENV NEO_LINK_MODE="rtsp" \
    NEO_LINK_PORT=8554 \
    MEDIAMTX_API_PORT=9997

CMD /usr/local/bin/neolink "${NEO_LINK_MODE}" --config /etc/neolink.toml
ENTRYPOINT ["/entrypoint.sh"]

# Expose RTSP port (MediaMTX) and API port
EXPOSE ${NEO_LINK_PORT} ${MEDIAMTX_API_PORT}
