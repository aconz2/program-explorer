# okay this thing works
# qemu-system-x86_64 \
#     -enable-kvm \
#     -cpu host -smp 4 -m 1G \
#     -kernel vmlinuz-6.8.5-301.fc40.x86_64 -append "root=/dev/vda4 rootflags=subvol=root" \
#     -initrd initramfs-6.8.5-301.fc40.x86_64.img \
#     -device virtio-blk-pci,drive=test \
#     -drive id=test,file=fedora-cloud-base.raw,format=raw,if=none

# and this works with nographic
# qemu-system-x86_64 \
#     -enable-kvm \
#     -cpu host -smp 4 \
#     -m 1G \
#     -kernel vmlinuz-6.8.5-301.fc40.x86_64 -append "root=/dev/vda4 console=ttyS0 rootflags=subvol=root" \
#     -initrd initramfs-6.8.5-301.fc40.x86_64.img \
#     -device virtio-blk-pci,drive=test \
#     -drive id=test,file=fedora-cloud-base.raw,format=raw,if=none \
#     -nodefaults -no-user-config -nographic \
#     -serial stdio
    
# now the microvm case
    #-M microvm,x-option-roms=off,pit=off,pic=off,isa-serial=off,rtc=off \
# x-option-roms=off is what causes there to be no console output anymore
qemu-system-x86_64 \
    -M microvm,x-option-roms=off,pit=off,pic=off,isa-serial=off,rtc=off \
    -nodefaults -no-user-config -nographic \
    -enable-kvm \
    -cpu host -smp 4 -m 1G \
    -kernel vmlinuz-6.8.5-301.fc40.x86_64 -append "root=/dev/vda4 console=hvc0 rootflags=subvol=root" \
    -initrd initramfs-6.8.5-301.fc40.x86_64.img \
    -device virtio-blk-device,drive=test \
    -drive id=test,file=fedora-cloud-base.raw,format=raw,if=none \
    -chardev stdio,id=virtiocon0 \
    -device virtio-serial-device \
    -device virtconsole,chardev=virtiocon0
