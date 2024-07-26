k=/home/andrew/Repos/linux/vmlinux

# -S pauses the cpu at startup

    # -S \
qemu-system-x86_64 \
    -nographic \
    -gdb tcp::1234 \
    -enable-kvm \
    -device pvpanic-pci \
    -cpu host -smp 1 -m 1G \
    -kernel $k -append "console=ttyS0" \
    -initrd init1.initramfs $@
