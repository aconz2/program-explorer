# https://github.com/kata-containers/kata-containers/releases/download/3.6.0/kata-static-3.6.0-amd64.tar.xz
d=$(realpath ~/Downloads/kata-static-3.6.0-amd64/kata/share/kata-containers)

# boots very fast!

qemu-system-x86_64 \
    -M microvm,pit=off,pic=off,isa-serial=off,rtc=off \
    -nodefaults -no-user-config -nographic \
    -enable-kvm \
    -cpu host -smp 4 -m 1G \
    -kernel $d/vmlinux-6.1.62-132 -append "console=hvc0 rdinit=/bin/bash" \
    -initrd $d/kata-alpine-3.18.initrd \
    -chardev stdio,id=virtiocon0 \
    -device virtio-serial-device \
    -device virtconsole,chardev=virtiocon0
