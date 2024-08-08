k=/home/andrew/Repos/linux/vmlinux

# exit vsockhello procs when we ctrl-c
trap "pkill -P $$" EXIT

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

    #--pmem file=gcc-14.1.0.sqfs,discard_writes=on \
#strace --decode-pids=comm -f ./cloud-hypervisor-static \
time ./cloud-hypervisor-static \
    --kernel $k \
    --initramfs initramfs \
    --serial off \
    --pmem file=pmemtestfile \
    --disk path=gcc-14.1.0.sqfs,readonly=on,id=gcc14 \
    --cmdline "quiet console=hvc0" \
    --cpus boot=1 \
    --memory size=1024M \
    --vsock cid=3,socket=/tmp/ch.sock $@

#wait

echo '---------from host--------------'
ls /tmp/_out
for x in /tmp/_out/*; do
    echo "------------- ${x} -----------------"
    cat ${x}
    echo '-----------------------------------'
done

x=pmemtestfile
# okay but why does cat truncate the output?
echo "------------- ${x} -----------------"
cat ${x}
echo '-----------------------------------'

