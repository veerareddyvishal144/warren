# warren/agent: the built-in agent. The gateway's `openai` driver runs an agent
# loop (shell, read/write file, list dir tools) against any OpenAI-compatible
# chat-completions endpoint.
#
# No secrets are baked in: OPENAI_BASE_URL, OPENAI_API_KEY and WARREN_MODEL are
# supplied per instance through the instance env. Contract: images/common/README.md.
#
#   docker build -f images/agent/Dockerfile --build-arg WARREN_IMAGE_TAG=dev -t warren/agent:dev images

ARG WARREN_IMAGE_TAG=dev
FROM warren/base:${WARREN_IMAGE_TAG}

ENV WARREN_AGENT_DRIVER=openai
