# warren/opencode: OpenCode, the open-source coding agent, behind the Agent API.
# The gateway's `command` driver runs `opencode run <prompt>` once per turn in
# the home directory and returns stdout as the reply.
#
# OpenCode is installed from the `opencode-ai` npm package (its platform
# binary ships as an optional dependency). Provider credentials such as
# OPENAI_API_KEY or ANTHROPIC_API_KEY come from the instance env; nothing is
# baked in. Contract: images/common/README.md.
#
#   docker build -f images/opencode/Dockerfile --build-arg WARREN_IMAGE_TAG=dev -t warren/opencode:dev images

ARG WARREN_IMAGE_TAG=dev
FROM warren/base:${WARREN_IMAGE_TAG}

# npm dist-tag or version of opencode-ai, e.g. 1.19.5.
ARG OPENCODE_VERSION=latest

# Installed as root under /usr/local (the default npm prefix), which every user
# can execute. The package's postinstall picks the platform binary from its
# optional dependencies (the AVX2 build when the build host has AVX2, else the
# baseline build) and hard-links it to bin/opencode.exe; it is run explicitly
# because recent npm versions skip install scripts of global packages. The
# per-platform packages are then removed: npm installs both the AVX2 and the
# baseline variant (about 200 MB each) and only the linked binary is needed.
# Caches and the files a version check leaves under /home/agent are removed
# because the persistent disk replaces that directory at runtime anyway.
# (cd is scoped to a subshell; `head | grep -q ELF` fails the step when the
# binary is missing or not native.)
# hadolint ignore=DL3003,DL4006
RUN set -eux; \
    npm install -g --no-audit --no-fund --ignore-scripts "opencode-ai@${OPENCODE_VERSION}"; \
    pkg=/usr/local/lib/node_modules/opencode-ai; \
    (cd "$pkg" && node postinstall.mjs); \
    head -c 4 "$pkg/bin/opencode.exe" | grep -q ELF; \
    rm -rf "$pkg"/node_modules/opencode-*; \
    rm -rf /root/.npm /root/.cache /tmp/*; \
    test -x /usr/local/bin/opencode; \
    opencode --version; \
    su agent -c 'cd /home/agent && opencode --version'; \
    rm -rf /home/agent/.cache /home/agent/.config /home/agent/.local

ENV WARREN_AGENT_DRIVER=command \
    WARREN_AGENT_COMMAND='opencode run {prompt}'
