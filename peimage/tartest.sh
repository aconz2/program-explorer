#!/bin/bash

set -e

which podman

ocidir=/mnt/storage/program-explorer/ocidir
image=${1:-index.docker.io/library/gcc:13.3.0}

echo "img is $image"

function my-podman-export() {
    id=$(podman create $1)
    trap "podman rm $id" EXIT
    podman export $id
}

# echo "checking left=peimage(go) right=squash-oci"
# cargo run --release --bin tardiff -- \
#     <(peimage export-notf $ocidir $image) \
#     <(cargo run --release --bin squash-oci -- $ocidir $image)
#
echo "=================================================="

# if the tag is the same but the sha is different, may need to

echo "checking left=podman right=squash-oci"
cargo run --release --bin tardiff -- \
    <(my-podman-export $image) \
    <(cargo run --release --bin squash-oci -- $ocidir $image)

echo "if things are different, maybe try checking again with"
echo "skopeo copy oci:$ocidir:$image containers-storage:$image"
