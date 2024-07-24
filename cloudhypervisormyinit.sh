k=/home/andrew/Repos/linux/vmlinux

# this worked
./cloud-hypervisor-static \
    --kernel $k \
    --initramfs init1.initramfs \
    --cmdline "console=hvc0 reboot=t" \
    --disk path=init1-ext4.img,readonly=on,id=root \
    --disk path=gcc-squashfs.sqfs,readonly=on,id=container-bundle-squashfs \
    --cpus boot=1 \
    --memory size=1024M \
    --vsock cid=3,socket=/tmp/ch.sock
