k=/home/andrew/Repos/linux/vmlinux

    #--console off \

#strace --decode-pids=comm --trace=!ioctl,close,mmap,munmap,io_uring_enter -f -o chstrace.out ./cloud-hypervisor-static \

rm -f /tmp/ch.sock*

rm -rf /tmp/_out
mkdir /tmp/_out

echo 'hi' > /tmp/_stdin
#strace -o stdinsender.straceout 
./vsockhello u/tmp/ch.sock_123 1 cat < /tmp/_stdin &
./vsockhello u/tmp/ch.sock_124 0 cpio -i -D /tmp/_out &
#./vsockhello u/tmp/ch.sock_124 0 cat > /tmp/_out.cpio &

#strace --decode-pids=comm -f ./cloud-hypervisor-static \
./cloud-hypervisor-static \
    --kernel $k \
    --initramfs initramfs \
    --serial off \
    --cmdline "quiet console=hvc0" \
    --disk path=gcc-14.1.0.sqfs,readonly=on \
    --cpus boot=1 \
    --memory size=1024M \
    --vsock cid=3,socket=/tmp/ch.sock $@

wait

echo '---------from host--------------'
ls /tmp/_out
for x in /tmp/_out/*; do
    echo "------------- ${x} -----------------"
    cat ${x}
    echo '-----------------------------------'
done
