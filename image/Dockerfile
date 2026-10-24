# syntax=docker/dockerfile:1
# supersapi image: the sapi image plus the swarm extension.
# Built by `supersapi build` with this directory as context.
FROM node:24-bookworm-slim

ARG PI_VERSION=latest

ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    TERM=xterm-256color \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false \
    NPM_CONFIG_AUDIT=false

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash ca-certificates curl git jq less openssh-client procps ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && git config --system --add safe.directory '*' \
    && mkdir -p /home/sapi /pi-config /pi-sessions /run/sapid

RUN npm install --global --ignore-scripts "@earendil-works/pi-coding-agent@${PI_VERSION}" \
    && npm cache clean --force \
    && pi --version

# uv gives the agent Python without touching the host. Two static binaries.
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /usr/local/bin/

# The swarm extension. Loaded explicitly with `pi --extension`, never by
# discovery, so the shared config volume is left untouched.
COPY extension/ /opt/supersapi/extension/

# Entrypoint: optional self-update, then drop to the host user (Linux) so files
# written to /workspace are owned by you, not root.
COPY <<'ENTRYPOINT_EOF' /usr/local/bin/sapi-entrypoint
#!/bin/bash
set -eu
if [ "${SAPI_AUTO_UPDATE:-1}" = "1" ]; then
    pi update self --no-approve >&2 \
        || printf 'sapi: pi update failed; using installed pi %s\n' "$(pi --version 2>/dev/null)" >&2
fi
uid="${SAPI_UID:-0}"
gid="${SAPI_GID:-0}"
if [ "$uid" != "0" ]; then
    for dir in /pi-config /pi-sessions /home/sapi; do
        [ "$(stat -c '%u:%g' "$dir")" = "$uid:$gid" ] || chown -R "$uid:$gid" "$dir"
    done
    exec setpriv --reuid "$uid" --regid "$gid" --clear-groups \
        env HOME=/home/sapi USER=sapi pi "$@"
fi
exec pi "$@"
ENTRYPOINT_EOF
RUN chmod 0755 /usr/local/bin/sapi-entrypoint

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/sapi-entrypoint"]
