    #-kernel $d/vmlinux-6.1.62-132 -append "console=hvc0 rdinit=/bin/bash" \
    #-initrd $d/kata-alpine-3.18.initrd \
    # -chardev stdio,id=virtiocon0 \
    # -device virtio-serial-device \
    # -device virtconsole,chardev=virtiocon0
    #-nodefaults -no-user-config \
    #
# qemu-img create -f qcow2 raw_hdd_10.qcow2 10G
# -hda alpine/raw_hdd_10.qcow2

# qemu-system-x86_64 \
#     -enable-kvm \
#     -nographic \
#     -cpu host -smp 1 -m 1G \
#     -cdrom alpine/alpine-virt-3.20.1-x86_64.iso

qemu-system-x86_64 \
    -M microvm,pit=off,pic=off,isa-serial=off,rtc=off \
    -nodefaults -no-user-config -nographic \
    -enable-kvm \
    -cpu host -smp 1 -m 1G \
    -kernel alpine/alpine-virt-vmlinuz -append "console=hvc0" \
    -initrd alpine/alpine-virt-initramfs \
    -device virtio-serial-device \
    -device virtconsole,chardev=virtiocon0 \
    -chardev stdio,id=virtiocon0
