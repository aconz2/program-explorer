# wget https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/cloud/nocloud_alpine-3.20.1-x86_64-uefi-tiny-r0.qcow2
# virt-get-kernel -a nocloud_alpine-3.20.1-x86_64-uefi-tiny-metal-r0.qcow2
# qemu-img convert nocloud_alpine-3.20.1-x86_64-uefi-tiny-metal-r0.qcow2 nocloud_alpine-3.20.1.raw
#
qemu-system-x86_64 \
    -M microvm,pit=off,pic=off,isa-serial=off,rtc=off \
    -nodefaults -no-user-config -nographic \
    -enable-kvm \
    -cpu host -smp 1 -m 1G \
    -kernel alpine/vmlinux-lts -append "earlyprintk=hvc0 console=hvc0 root=/dev/vda2 ext4" \
    -initrd alpine/initramfs-lts.nogz \
    -device virtio-blk-device,drive=test \
    -drive id=test,file=alpine/nocloud_alpine-3.20.1.raw,format=raw,if=none \
    -chardev stdio,id=virtiocon0 \
    -device virtio-serial-device \
    -device virtconsole,chardev=virtiocon0

    #-initrd alpine/initramfs-lts \
    # -device virtio-blk-device,drive=test \
    # -drive id=test,file=alpine/nocloud_alpine-3.20.1.raw,format=raw,if=none \

# → bash alpinetest.sh 
# [    1.342319] FAT-fs (vda1): utf8 is not a recommended IO charset for FAT filesystems, filesystem will be case sensitive!
# [    1.397878] EXT4-fs (vda2): mounted filesystem 1aaef596-4ea8-4f43-95f3-089262e88d90 ro with ordered data mode. Quota mode: none.
# [    1.401545] EXT4-fs (vda2): unmounting filesystem 1aaef596-4ea8-4f43-95f3-089262e88d90.
# [    6.412747] Mounting boot media: failed. 
# [    6.447873] Installing packages to root filesystem...
# [    6.457578] Installing packages to root filesystem: ok.
# switch_root: can't execute '/sbin/init': No such file or directory
# [    6.486978] Kernel panic - not syncing: Attempted to kill init! exitcode=0x00000100
# [    6.487120] CPU: 0 PID: 1 Comm: switch_root Not tainted 6.6.34-1-lts #2-Alpine
# [    6.487257] Hardware name: Bochs Bochs, BIOS Bochs 01/01/2011
# [    6.487371] Call Trace:
# [    6.487418]  <TASK>
# [    6.487463]  dump_stack_lvl+0x47/0x70
# [    6.487548]  panic+0x180/0x340
# [    6.487644]  do_exit+0x98d/0xb00
# [    6.487715]  do_group_exit+0x31/0x80
# [    6.487798]  __x64_sys_exit_group+0x18/0x20
# [    6.487865]  do_syscall_64+0x5a/0x90
# [    6.487940]  entry_SYSCALL_64_after_hwframe+0x78/0xe2
# [    6.488041] RIP: 0033:0x7fa98ccfac07
# [    6.488109] Code: 8b 76 28 48 89 c7 e9 01 44 00 00 64 48 8b 04 25 00 00 00 00 48 8b b0 a8 00 00 00 e9 bf ff ff ff 48 63 ff b8 e7 00 00 00 0f 8
# [    6.488425] RSP: 002b:00007ffebf5b6c98 EFLAGS: 00000246 ORIG_RAX: 00000000000000e7
# [    6.488563] RAX: ffffffffffffffda RBX: 0000000000000001 RCX: 00007fa98ccfac07
# [    6.488708] RDX: 00007fa98ccfae9f RSI: 0000000000000000 RDI: 0000000000000001
# [    6.488845] RBP: 0000555a8cc3a9de R08: 0000000000000000 R09: 0000000000000000
# [    6.488982] R10: 0000000000000000 R11: 0000000000000246 R12: 0000555a8cc3475d
# [    6.489118] R13: 0000555a8cc39d82 R14: 0000000000000002 R15: 00007ffebf5b6fa0
# [    6.489260]  </TASK>
# [    6.489448] Kernel Offset: 0x3b000000 from 0xffffffff81000000 (relocation range: 0xffffffff80000000-0xffffffffbfffffff)
# [    6.489641] ---[ end Kernel panic - not syncing: Attempted to kill init! exitcode=0x00000100 ]---
