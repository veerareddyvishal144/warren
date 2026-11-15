# warren/base: the plain Linux computer every Warren template builds on.
# Contract: images/common/README.md. Built by scripts/build-images.sh with the
# images/ directory as the build context:
#
#   docker build -f images/base/Dockerfile -t warren/base:dev images
#
# Plain Dockerfile syntax only (no heredocs, no RUN --mount) so it builds with
# both the legacy builder and BuildKit.

# Node comes from the official image of the same Debian release (same glibc).
ARG NODE_IMAGE=node:24-bookworm-slim
FROM ${NODE_IMAGE} AS node

FROM debian:bookworm-slim

ARG DEBIAN_FRONTEND=noninteractive
# ttyd release to install: "latest" follows the GitHub latest-release redirect;
# pin with --build-arg TTYD_VERSION=1.7.7 for reproducible builds.
ARG TTYD_VERSION=latest

# WARREN_HOME and WARREN_GATEWAY_PORT are also injected per instance by the
# daemon; these defaults make a bare `docker run` behave the same way.
# PIP_BREAK_SYSTEM_PACKAGES lets `pip install` work on Debian's system Python
# (PEP 668) without a venv; as the agent user pip falls back to --user.
ENV LANG=C.UTF-8 \
    WARREN_HOME=/home/agent \
    WARREN_GATEWAY_PORT=3737 \
    PIP_BREAK_SYSTEM_PACKAGES=1 \
    PATH=/home/agent/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# System packages. --no-install-recommends and a cleaned apt cache keep the
# image inside its 700 MB budget.
RUN set -eux; \
    [ "$(dpkg --print-architecture)" = "amd64" ] || { echo "warren/base is x86_64 only" >&2; exit 1; }; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        python3 \
        python3-pip \
        python3-venv \
        procps \
        tar \
        unzip \
        jq \
        sudo \
        openssh-client \
        build-essential \
    ; \
    apt-get clean; \
    rm -rf /var/lib/apt/lists/*; \
    ln -s /usr/bin/python3 /usr/local/bin/python; \
    python3 --version; \
    python3 -m pip --version; \
    git --version

# Node 24 and npm from the official image: the node binary, the npm module and
# the usual npm/npx symlinks.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN set -eux; \
    ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm; \
    ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx; \
    node --version; \
    npm --version

# The instance user: uid 1000, gid 1000, home /home/agent (the persistent disk
# is bind-mounted there at runtime), passwordless sudo.
RUN set -eux; \
    groupadd -g 1000 agent; \
    useradd -m -u 1000 -g 1000 -s /bin/bash agent; \
    echo 'agent ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/agent; \
    chmod 0440 /etc/sudoers.d/agent; \
    visudo -cf /etc/sudoers.d/agent

# ttyd: the static x86_64 binary of the GitHub release, verified against the
# SHA256SUMS published with the same release, then checked to run.
# (cd is scoped to this RUN; a non-matching grep leaves sha256sum no checksum
# lines, which fails the step on its own.)
# hadolint ignore=DL3003,DL4006
RUN set -eux; \
    if [ "$TTYD_VERSION" = "latest" ]; then \
        base="https://github.com/tsl0922/ttyd/releases/latest/download"; \
    else \
        base="https://github.com/tsl0922/ttyd/releases/download/${TTYD_VERSION}"; \
    fi; \
    cd /tmp; \
    curl -fsSL --retry 3 --retry-delay 2 -o ttyd.x86_64 "$base/ttyd.x86_64"; \
    curl -fsSL --retry 3 --retry-delay 2 -o SHA256SUMS "$base/SHA256SUMS"; \
    grep ' ttyd.x86_64$' SHA256SUMS | sha256sum -c -; \
    install -m 0755 ttyd.x86_64 /usr/local/bin/ttyd; \
    rm -f ttyd.x86_64 SHA256SUMS; \
    ttyd --version

# The in-instance gateway: Agent API, files API, exec, health and ttyd
# supervision. scripts/build-images.sh stages the release binary at
# images/.build/warren-gateway (the build context is images/).
COPY .build/warren-gateway /usr/local/bin/warren-gateway
RUN chmod 0755 /usr/local/bin/warren-gateway

WORKDIR /home/agent

# 3737: gateway (Agent API, files, exec, health). 7681: ttyd browser terminal.
EXPOSE 3737 7681

# The container runs as root on purpose: the gateway drops to `agent` for ttyd
# and honours the user requested on exec.
ENTRYPOINT ["/usr/local/bin/warren-gateway", "--serve"]
