#!/usr/bin/env bash
# Container entrypoint: bridge the factory's egress proxy, then run the command.
#
# The container has no network. When the factory mounts its proxy socket at
# /run/factory/proxy.sock, socat listens on 127.0.0.1:3128 and forwards to
# it; HTTPS_PROXY and CARGO_HTTP_PROXY already point there. Without the
# socket the command runs fully offline.
set -euo pipefail

SOCKET=/run/factory/proxy.sock
if [ -S "$SOCKET" ]; then
    socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:"$SOCKET" &
fi

exec "$@"
