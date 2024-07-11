#!/usr/bin/env bash

qemu-system-x86_64 \
   -M microvm,x-option-roms=off,pit=off,pic=off,isa-serial=off,rtc=off \
   -enable-kvm -cpu host -m 512m -smp 2 \
   -nodefaults -no-user-config -nographic \
   -chardev stdio,id=virtiocon0 \
   -device virtio-serial-device \
   -device virtconsole,chardev=virtiocon0 \
   -drive file=~/Downloads/Fedora-Cloud-Base-Generic.x86_64-40-1.14.qcow2

   #-append "earlyprintk=hvc0 console=hvc0 root=/dev/vda1" \
# -netdev tap,id=tap0,script=no,downscript=no \
# -device virtio-blk-device,drive=test \
# -device virtio-net-device,netdev=tap0 \
#    -drive id=test,file=test.img,format=qcow2,if=none \
   #-initrd ubuntu-21.04-server-cloudimg-amd64-initrd-generic \
   #-kernel ubuntu-21.04-server-cloudimg-amd64-vmlinuz-generic \
