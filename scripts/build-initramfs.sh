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

function gen_file() {
    sed \
    -e "s/\$PROFILE/$profile/" \
    -e "s!\$CRUN!$crun!" \
    initramfs.file | \
    (if [[ "$profile" = "release" ]];
        # this one removes the whole line
        then sed -e "s/.*#@ REMOVE_IN_RELEASE//";
        # this one removes trailing whitespace and the marker
        # gen_init_cpio doesn't like having anything else in the line
        else sed -e "s/ *#@ REMOVE_IN_RELEASE//";
    fi)
    # the
}

echo "=========== using initrams.file =========="
gen_file
echo "=========================================="

./vendor/gen_init_cpio <(gen_file) > $outfile

echo "wrote to $outfile"
ls -lh $outfile
cpio -vt < $outfile
