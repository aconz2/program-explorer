k=/home/andrew/Repos/linux/vmlinux
ch=/home/andrew//Repos/cloud-hypervisor/target/x86_64-unknown-linux-musl/release/cloud-hypervisor
ch=/home/andrew/Repos/cloud-hypervisor/target/x86_64-unknown-linux-musl/debug/cloud-hypervisor
ch=/home/andrew/Repos/cloud-hypervisor/target/x86_64-unknown-linux-musl/profiling/cloud-hypervisor
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
#     --disk path=gcc-14.1.0.sqfs,readonly=on,id=container-bundle-squashfs \
#     --cpus boot=1 \
#     --memory size=1024M
# 
# 

# needs sudo
# perf stat -e 'kvm:*' $ch \
#perf record --freq 5000 $ch \
#strace -f --absolute-timestamps=format:unix,precision:us -o strace.out --trace=!ioctl,close $ch \
#perf record --freq 5000 --call-graph dwarf $ch \
#perf record --call-graph lbr --all-user --user-callchains -g \
#perf record --freq 10000 -g \
    $ch \
     --seccomp log \
     --kernel $k \
     --initramfs initramfs \
     --console off \
     --cmdline "console=hvc0" \
     --disk path=gcc-14.1.0.sqfs,readonly=on,id=container-bundle-squashfs \
     --cpus boot=1 \
     --memory size=1024M

#python3 make_strace_relative_time.py strace.out
#cat strace.out
