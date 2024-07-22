k=/home/andrew/Repos/linux/vmlinux

# this worked
./cloud-hypervisor-static \
    --kernel $k \
    --initramfs init1.initramfs \
    --cmdline "console=hvc0" \
    --cpus boot=1 \
    --memory size=1024M

