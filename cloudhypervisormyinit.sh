k=/home/andrew/Repos/linux/vmlinux

    #--console off \

#strace --decode-pids=comm --trace=!ioctl,close,mmap,munmap,io_uring_enter -f -o chstrace.out ./cloud-hypervisor-static \

rm -f /tmp/ch.sock*

echo 'hi' > /tmp/_stdin
#strace -o stdinsender.straceout 
./vsockhello u/tmp/ch.sock_123 1 cat < /tmp/_stdin &

./cloud-hypervisor-static \
    --kernel $k \
    --initramfs initramfs \
    --serial off \
    --cmdline "quiet console=hvc0" \
    --disk path=gcc-14.1.0.sqfs,readonly=on \
    --cpus boot=1 \
    --memory size=1024M \
    --vsock cid=3,socket=/tmp/ch.sock $@
