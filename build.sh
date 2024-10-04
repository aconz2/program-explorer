#!/bin/bash

set -e

for dir in pearchive peinit; do
    (cd $dir && cargo build --target x86_64-unknown-linux-musl)
done

./makeinitramfs.sh

ls -lh initramfs

(cd perunner && cargo build)
