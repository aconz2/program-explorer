k=/home/andrew/Repos/linux/vmlinux

# this worked
exec ./cloud-hypervisor-static \
    --kernel $k \
    --initramfs initramfs \
    --cmdline "console=hvc0" \
    --disk path=gcc-squashfs.sqfs,readonly=on,id=container-bundle-squashfs \
    --cpus boot=1 \
    --memory size=1024M \
    --vsock cid=3,socket=/tmp/ch.sock $@
