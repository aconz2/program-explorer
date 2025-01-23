#!/bin/bash

set -e

profile=${1:-debug}
mkdir -p target/$profile

# for whatever reason you have to use --profile=dev to get ./target/debug/...
if [[ "$profile" == "debug" ]]; then
    cargo_profile="dev"
else
    cargo_profile="$profile"
fi

for dir in perunner peserver; do
    (cd $dir && cargo build --profile=${cargo_profile})
done

for dir in peinit pearchive; do
    (cd $dir && cargo build --profile=${cargo_profile} --target x86_64-unknown-linux-musl)
done

./makeinitramfs.sh "$profile" > target/$profile/initramfs
