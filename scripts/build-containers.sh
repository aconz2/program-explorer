#!/usr/bin/bash

set -e

./scripts/build.sh release

tag=latest

podman build -t pe-server-lb:$tag -f containers/pe-server-lb .

# ugh copy of symlink won't work, should really build this in a container or something
cp vmlinux target/release/vmlinux
podman build -t pe-server-worker:$tag -f containers/pe-server-worker .

