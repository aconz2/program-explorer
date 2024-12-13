#!/usr/bin/env bash

peimage="./peimage"
storage=/var/tmp/program-explorer-ocidir

mkdir -p storage

if [ "$1" = "busybox" ]; then
    refs=()
    for tag in 1.37 1.36.1 1.36.0; do
        refs+=(index.docker.io/library/busybox:$tag)
    done

    $peimage pull $storage ${refs[@]}
    $peimage image busybox.erofs $storage ${refs[@]}

elif [ "$1" = "gcc" ]; then
    refs=()
    for tag in 13.3.0 14.1.0; do
        refs+=(index.docker.io/library/gcc:$tag)
    done

    $peimage pull $storage ${refs[@]}
    $peimage image gcc.erofs $storage ${refs[@]}

elif [ "$1" = "ffmpeg" ]; then
    refs=(
        "index.docker.io/chainguard/ffmpeg:sha256-75f852da4ee623c16035f886d1ac0391d32319a0a5a6e741c0017ae8726d532f"
    )

    $peimage pull $storage ${refs[@]}
    $peimage image ffmpeg.erofs $storage ${refs[@]}
fi
