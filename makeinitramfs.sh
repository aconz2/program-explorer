#!/bin/bash

profile=${1:-debug}
echo "using profile $profile in initramfs"

~/Repos/linux/usr/gen_init_cpio <(sed "s/PROFILE/${profile}/" initramfs.file) > initramfs

# size=$(stat --format='%s' init1.initramfs)
# size=$(($size / 1024 / 1024 + 10))
# tmpdir=$(mktemp -d)
# trap "sudo rm -rf $tmpdir" EXIT
# # need sudo with mknod
# sudo cpio -v --extract --directory $tmpdir < init1.initramfs
# sudo mkfs.ext4 -F -d $tmpdir init1-ext4.img "${size}m"
