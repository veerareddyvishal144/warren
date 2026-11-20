# warren/claude: the plain Linux computer with the coding-agent CLIs installed:
# Claude Code (@anthropic-ai/claude-code), Codex (@openai/codex) and OpenCode
# (opencode-ai). No Agent API driver is set: the CLIs are driven over exec or
# the browser terminal as the `agent` user, with provider credentials such as
# ANTHROPIC_API_KEY or OPENAI_API_KEY supplied through the instance env.
# Nothing is baked in. Contract: images/common/README.md.
#
#   docker build -f images/claude/Dockerfile --build-arg WARREN_IMAGE_TAG=dev -t warren/claude:dev images

ARG WARREN_IMAGE_TAG=dev
FROM warren/base:${WARREN_IMAGE_TAG}

# npm dist-tags or versions, e.g. 2.1.329, 0.170.0, 1.19.6.
ARG CLAUDE_CODE_VERSION=latest
ARG CODEX_VERSION=latest
ARG OPENCODE_VERSION=latest

# Node LTS (24) and npm come from warren/base. The packages are installed as
# root under /usr/local (the default npm prefix), which every user can execute.
# claude-code and opencode-ai choose their x86_64 binary in a postinstall hook
# (install.cjs, postinstall.mjs) and hard-link it to bin/<name>.exe; the hooks
# run explicitly because npm 11 warns about install scripts it has not been
# told to allow and a later version may skip them. Their per-platform packages
# are removed afterwards (the hard link keeps the binary; npm also pulls the
# baseline and musl variants, 150 to 350 MB each); codex's platform package
# stays because bin/codex.js resolves it at run time. The version checks run
# as root and as agent; the files they leave under /home/agent are removed
# because the persistent disk replaces that directory at runtime anyway.
# (`head | grep -q ELF` fails the step when a binary is missing or not native.)
# hadolint ignore=DL3003,DL4006
RUN set -eux; \
    npm install -g --no-audit --no-fund --ignore-scripts \
        "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
        "@openai/codex@${CODEX_VERSION}" \
        "opencode-ai@${OPENCODE_VERSION}"; \
    mods=/usr/local/lib/node_modules; \
    (cd "$mods/@anthropic-ai/claude-code" && node install.cjs); \
    (cd "$mods/opencode-ai" && node postinstall.mjs); \
    head -c 4 "$mods/@anthropic-ai/claude-code/bin/claude.exe" | grep -q ELF; \
    head -c 4 "$mods/opencode-ai/bin/opencode.exe" | grep -q ELF; \
    rm -rf "$mods/@anthropic-ai/claude-code/node_modules/@anthropic-ai"; \
    rm -rf "$mods"/opencode-ai/node_modules/opencode-*; \
    test -x /usr/local/bin/claude; \
    test -x /usr/local/bin/codex; \
    test -x /usr/local/bin/opencode; \
    claude --version; \
    codex --version; \
    opencode --version; \
    su agent -c 'cd /home/agent && claude --version && codex --version && opencode --version'; \
    rm -rf /root/.npm /root/.cache /root/.config /root/.local /root/.claude /root/.claude.json /root/.codex /tmp/*; \
    rm -rf /home/agent/.cache /home/agent/.config /home/agent/.local /home/agent/.claude /home/agent/.claude.json /home/agent/.codex
