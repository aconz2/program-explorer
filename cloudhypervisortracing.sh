k=/home/andrew/Repos/linux/vmlinux
ch=~/Repos/cloud-hypervisor/target/x86_64-unknown-linux-musl/release/cloud-hypervisor
ch=~/Repos/cloud-hypervisor/target/x86_64-unknown-linux-musl/debug/cloud-hypervisor
#ch=~/Repos/cloud-hypervisor/target/debug/cloud-hypervisor

set -e

#strace -o /tmp/strace.out -f $ch \
    #--seccomp log --log-file ch.log \
#strace --decode-pids=comm -f $ch
#strace --stack-traces -f --absolute-timestamps=format:unix,precision:us -o strace.out $ch \
#$ch \

# strace -f --absolute-timestamps=format:unix,precision:us -o strace.out $ch \
#     --seccomp log \
#     --kernel $k \
#     --initramfs initramfs \
#     --cmdline "console=hvc0 tp_printk trace_event=initcall:*" \
#     --disk path=gcc-squashfs.sqfs,readonly=on,id=container-bundle-squashfs \
#     --cpus boot=1 \
#     --memory size=1024M
# 
# 
# python3 make_strace_relative_time.py strace.out
# cat strace.out

#perf record --call-graph fp $ch \
perf stat -e 'kvm:*' $ch \
    --seccomp log \
    --kernel $k \
    --initramfs initramfs \
    --cmdline "console=hvc0 tp_printk trace_event=initcall:*" \
    --disk path=gcc-squashfs.sqfs,readonly=on,id=container-bundle-squashfs \
    --cpus boot=1 \
    --memory size=1024M
