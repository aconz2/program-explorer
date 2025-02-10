#!/bin/bash

set -e

profile=${1:-debug}
crun=${CRUN:-vendor/crun-1.15-linux-amd64}

echo "using profile=$profile crun=$crun" 1>2

# TODO figure out a better way to refer to this tool (or vendor or rewrite it)
# sed with ! separator to avoid problem of var subs with / in it... bad bad bad
../linux/usr/gen_init_cpio <(
  sed \
    -e "s/\$PROFILE/$profile/" \
    -e "s!\$CRUN!$crun!" \
    -e "s/.*#! REMOVE_IN_RELEASE//" \
    initramfs.file)

