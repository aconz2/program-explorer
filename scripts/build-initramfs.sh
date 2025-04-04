#!/bin/bash

set -e

profile=${1:-debug}
crun=${CRUN}
crun_url=https://github.com/containers/crun/releases/download/1.20/crun-1.20-linux-amd64
outfile=target/$profile/initramfs

if [[ -z $crun || ! -f $crun ]]; then
    crun=vendor/$(basename $crun_url)
    if [ ! -f $crun ]; then
        (cd vendor && wget $crun_url)
    fi
    echo 'e19a9a35484f3c75567219a7b6a4a580b43a0baa234df413655f48db023a200e  vendor/crun-1.20-linux-amd64' | sha256sum -c
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
    > $outfile

echo "wrote to $outfile"
ls -lh $outfile
cpio -vt < $outfile
