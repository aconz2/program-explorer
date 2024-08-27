k=/home/andrew/Repos/linux/vmlinux
ch=${ch:-/home/andrew/Repos/cloud-hypervisor/target/x86_64-unknown-linux-musl/profiling/cloud-hypervisor}

# exit children when we ctrl-c
#trap "pkill -P $$" EXIT

#strace --decode-pids=comm --trace=!ioctl,close,mmap,munmap,io_uring_enter -f -o chstrace.out ./cloud-hypervisor-static \

rm -rf /tmp/_out
mkdir /tmp/_out
truncate -s 2M /tmp/_out/output

rm -rf /tmp/_in
mkdir -p /tmp/_in/dir
echo 'hello this is stdin' > /tmp/_in/stdin
echo 'this is the contents of file1' > /tmp/_in/dir/file1

(cd /tmp/_in && mksquashfs . input.sqfs -no-compression -no-xattrs -force-uid 0 -force-gid 0)
python makepmemsized.py /tmp/_in/input.sqfs

    #--disk path=gcc-14.1.0.sqfs,readonly=on,id=gcc14 \
#strace --decode-pids=comm -f ./cloud-hypervisor-static \
time $ch \
    --kernel $k \
    --initramfs initramfs \
    --serial off \
    --pmem file=gcc-14.1.0.sqfs,discard_writes=on \
           file=/tmp/_in/input.sqfs,discard_writes=on \
           file=/tmp/_out/output \
    --cmdline "console=hvc0" \
    --cpus boot=1 \
    --memory size=1024M,thp=on
    $@

cpio --list < /tmp/_out/output
# "sh", "-c", "echo 'into file' > /output/file1; echo 'to stdout'; echo 'to stderr' 1>&2"
