# syntax=docker/dockerfile:1@sha256:87999aa3d42bdc6bea60565083ee17e86d1f3339802f543c0d03998580f9cb89
#
# Runtime-only image: it COPYs a prebuilt static (musl) binary — no Rust toolchain
# in the build, so `docker buildx` stays fast across linux/amd64 + linux/arm64
# (TAD §6.4). The release pipeline (Task 3.7) builds the per-arch binaries into
# dist/<arch>/podspine; `docker buildx build --platform ...` selects by TARGETARCH.
#
# Base is Alpine: the binary is static musl, so no glibc is needed, and Alpine's
# ffmpeg keeps the image ~1/3 the size of debian-slim (TAD sanctions this "if size
# matters" — it does; the ≤180MB target rules out debian's full ffmpeg).
FROM alpine:3.24@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

# ffmpeg is the one runtime dependency; ca-certificates for outbound TLS if needed.
RUN apk add --no-cache ffmpeg ca-certificates

# Non-root system user; own /data before VOLUME so the anonymous volume Docker
# creates at runtime inherits podspine ownership (else it can't write the DB).
RUN adduser -D -H -u 10001 -s /sbin/nologin podspine \
 && mkdir -p /app /data \
 && chown podspine:podspine /app /data
WORKDIR /app

# Prebuilt static binary for the target architecture (amd64 or arm64).
ARG TARGETARCH
COPY dist/${TARGETARCH}/podspine /usr/local/bin/podspine

USER podspine
EXPOSE 8080

# Zero-config defaults: mount the library, keep data in a volume.
ENV PODSPINE_BIND=0.0.0.0:8080 \
    PODSPINE_LIBRARY=/library \
    PODSPINE_DATA_DIR=/data
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/podspine"]
