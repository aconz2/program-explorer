#!/bin/bash

set -e

profile=${1:-debug}
crun=${CRUN}
crun_url=https://github.com/containers/crun/releases/download/1.20/crun-1.20-linux-amd64

if [[ -z $crun || ! -f $crun ]]; then
    crun=target/$(basename $crun_url)
fi

if [ ! -f $crun ]; then
    (cd target && wget $crun_url)
fi

echo "using profile=$profile crun=$crun" 1>&2

if [ ! -f vendor/gen_init_cpio ]; then
    gcc -O1 vendor/gen_init_cpio.c -o vendor/gen_init_cpio
fi

./vendor/gen_init_cpio <(
  sed \
    -e "s/\$PROFILE/$profile/" \
    -e "s!\$CRUN!$crun!" \
    -e "s/.*#! REMOVE_IN_RELEASE//" \
    initramfs.file) \
    > target/$profile/initramfs

