k=/home/andrew/Repos/linux/vmlinux

# -S pauses the cpu at startup

    # -S \
    #-device pvpanic-pci \
qemu-system-x86_64 \
    -M microvm,pit=off,pic=off,isa-serial=off,rtc=off \
    -nographic -no-user-config -nodefaults \
    -gdb tcp::1234 \
    -enable-kvm \
    -cpu host -smp 1 -m 1G \
    -kernel $k -append "console=hvc0" \
    -device virtio-blk-device,drive=test \
    -drive id=test,file=gcc-squashfs.sqfs,read-only=on,format=raw,if=none \
    -initrd init1.initramfs \
    -chardev stdio,id=virtiocon0 \
    -device virtio-serial-device \
    -device virtconsole,chardev=virtiocon0 $@
