# lldb -o 'gdb-remote localhost:1234' -o 'break set -H -r ".*pivot_root.*"' ~/Repos/linux/vmlinux
# gdb -ex 'target remote localhost:1234' ~/Repos/linux/vmlinux -ex 'hbreak namespace.c:4197'

lldb -o 'gdb-remote localhost:1234' -o 'break set -H -f namespace.c -l 4197' ~/Repos/linux/vmlinux

