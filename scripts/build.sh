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

for package in perunner peserver; do
    cargo build --package=${package} --profile=${cargo_profile}
done

# todo would get this building in a container, but it seems caching deps locally is hard
# peserver with musl requires musl-gcc cmake and some compression things I think?
# idk how cmake enters the picture
# peimage requires erofs-utils (at runtime)

for package in peinit pearchive peserver; do
    cargo build --package=${package} --profile=${cargo_profile} --target x86_64-unknown-linux-musl
done

./scripts/build-initramfs.sh "$profile"
