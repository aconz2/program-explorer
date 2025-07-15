#!/usr/bin/bash

set -e

# disabling for now b/c I have to run this build step in a container,
# but then don't have access to podman inside the container
# ./scripts/build.sh release

tag=latest

podman build -t pe-server-lb:$tag -f containers/pe-server-lb .

# ugh copy of symlink won't work, should really build this in a container or something
cp vmlinux target/release/vmlinux
podman build -t pe-server-worker:$tag -f containers/pe-server-worker .

podman build -t pe-image-service:$tag -f containers/pe-image-service .

podman build -t pe-caddy:$tag -f containers/pe-caddy .
podman build -t pe-caddy-dev:$tag -f containers/pe-caddy-dev .

podman build -t pe-server-gh:$tag -f containers/pe-server-gh .
