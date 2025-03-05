#!/usr/bin/env bash

ocidir=/mnt/storage/program-explorer/ocidir
imagedir=/mnt/storage/program-explorer/images

# can use
#   dump.erofs -s -S <file.erofs>
# to view statistics

set -e

mkdir -p $imagedir

peimage image $imagedir/busybox.erofs $ocidir index.docker.io/library/busybox:{1.37,1.36.0,1.36.1}
peimage image $imagedir/gcc.erofs $ocidir index.docker.io/library/gcc:{13.3.0,14.1.0}

peimage image $imagedir/ffmpeg.erofs $ocidir \
    index.docker.io/chainguard/ffmpeg:sha256-75f852da4ee623c16035f886d1ac0391d32319a0a5a6e741c0017ae8726d532f

peimage image $imagedir/clang.erofs $ocidir index.docker.io/silkeh/clang:{17,19}
