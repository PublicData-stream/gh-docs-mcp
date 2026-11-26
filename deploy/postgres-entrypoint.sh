#!/bin/sh
set -eu

if [ "$(id -u):$(id -g)" != "70:70" ]; then
    echo 'PostgreSQL requires UID and GID 70' >&2
    exit 1
fi

exec /usr/local/bin/docker-entrypoint.sh "$@"
