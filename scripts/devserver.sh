#!/bin/bash

set -e

trap "trap - SIGTERM && kill -- -$$" SIGINT SIGTERM EXIT

export RUST_LOG=debug

cargo run --bin peimage-service -- --listen /tmp/image.sock --auth ~/Secure/container-registries.json &

cargo run --bin lb -- --uds /tmp/lb.sock --worker uds:/tmp/worker.sock &

env RUST_LOG=trace cargo run --bin worker -- --uds /tmp/worker.sock --image-service /tmp/image.sock --worker-cpuset 0:2:2 --kernel target/release/vmlinux --initramfs target/debug/initramfs --ch cloud-hypervisor-static &

(cd pefrontend && npm run dev -- --clearScreen=false) &

env RUNTIME_DIRECTORY=/tmp caddy run --config caddy/dev.caddyfile &

wait
