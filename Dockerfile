# The Warren daemon in a container: warrend, warren, warren-mcp and the static
# warren-gateway on a small Debian base, for a Linux host that runs Docker.
#
#   docker build -t warren:local .
#   docker compose -f packaging/docker-compose.yml up -d
#
# The container needs the host's network namespace, the Docker socket and the
# data directory mounted at the same path on both sides; packaging/docker-compose.yml
# has the working settings and docs/install.md ("Docker image") the reasons and the
# limits: gVisor and the template images live in the host's Docker, and the vm and
# mac instance types do not work from a container.
#
# Multi-stage: the build stage compiles with the official Rust image and is
# discarded; the runtime stage is debian:bookworm-slim plus ca-certificates,
# iptables (the daemon installs the instance egress rules and, as root, refuses
# to start without the tool) and curl (the compose healthcheck). Distroless is
# not an option while iptables is needed. Plain Dockerfile syntax (no heredocs,
# no RUN --mount) so it builds with the legacy builder and BuildKit, like
# images/base/Dockerfile. .dockerignore keeps target directories, data, images
# and docs out of the context.

ARG RUST_IMAGE=rust:1-bookworm
ARG BASE_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS build
ARG DEBIAN_FRONTEND=noninteractive
# hadolint ignore=DL3008
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends musl-tools file; \
    rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# Same recipe as scripts/release/package.sh: the three host binaries against
# glibc, the gateway as a static musl binary for the architecture being built
# (x86_64 or aarch64), linked with the musl-gcc wrapper from musl-tools.
RUN set -eux; \
    musl="$(uname -m)-unknown-linux-musl"; \
    rustup target add "$musl"; \
    cargo build --release --locked -p warrend -p warren-cli -p warren-mcp; \
    CC_x86_64_unknown_linux_musl=musl-gcc CC_aarch64_unknown_linux_musl=musl-gcc \
        cargo build --release --locked -p warren-gateway --target "$musl"; \
    mkdir -p /out; \
    cp target/release/warrend target/release/warren target/release/warren-mcp /out/; \
    cp "target/$musl/release/warren-gateway" /out/warren-gateway; \
    case "$(file /out/warren-gateway)" in *"dynamically linked"*) echo "warren-gateway must be static" >&2; exit 1 ;; esac; \
    /out/warrend --version; \
    /out/warren --version; \
    /out/warren-gateway --version

FROM ${BASE_IMAGE}
ARG DEBIAN_FRONTEND=noninteractive
# hadolint ignore=DL3008
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates iptables curl; \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /out/warrend /out/warren /out/warren-mcp /out/warren-gateway /usr/local/bin/
COPY packaging/docker/entrypoint.sh /usr/local/bin/warren-entrypoint
# Flags are WARREN_* variables (docs/self-hosting.md). WARREN_DATA_DIR must be
# the same path inside and outside the container; the entrypoint explains why.
ENV WARREN_DATA_DIR=/var/lib/warren \
    WARREN_LISTEN=:7337 \
    WARREN_PUBLIC_PORT=7337
EXPOSE 7337
ENTRYPOINT ["/usr/local/bin/warren-entrypoint"]
CMD ["warrend"]
