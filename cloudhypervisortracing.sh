k=/home/andrew/Repos/linux/vmlinux


set -e

# this worked
strace -f --absolute-timestamps=format:unix,precision:us -o strace.out ./cloud-hypervisor-static \
    --kernel $k \
    --initramfs initramfs \
    --cmdline "console=hvc0 tp_printk trace_event=initcall:*" \
    --disk path=gcc-squashfs.sqfs,readonly=on,id=container-bundle-squashfs \
    --cpus boot=1 \
    --memory size=1024M \
    --vsock cid=3,socket=/tmp/ch.sock $@


python3 make_strace_relative_time.py strace.out
cat strace.out
