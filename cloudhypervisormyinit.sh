k=/home/andrew/Repos/linux/vmlinux


exec ./cloud-hypervisor-static \
    --kernel $k \
    --initramfs initramfs \
    --serial off \
    --console off \
    --cmdline "quiet console=hvc0" \
    --disk path=gcc-14.sqfs,readonly=on,id=container-bundle-squashfs \
    --cpus boot=1 \
    --memory size=1024M \
    --vsock cid=3,socket=/tmp/ch.sock $@
