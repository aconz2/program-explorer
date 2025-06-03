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

# this is not really relevant to deploy
# for package in perunner; do
#     cargo build --package=${package} --profile=${cargo_profile}
# done

# todo would get this building in a container, but it seems caching deps locally is hard
# peserver with musl requires musl-gcc (cmake OR zlib-ng-devel)
# pingora requires flate2 with the zlib-ng feature
# peimage requires erofs-utils (at runtime)

for package in peinit pearchive peserver peimage-service; do
    cargo build --package=${package} --profile=${cargo_profile} --target x86_64-unknown-linux-musl
done

./scripts/build-initramfs.sh "$profile"

if [ "$profile" = "release" ]; then
    (cd pefrontend && npm run build)
fi
