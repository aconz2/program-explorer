# lldb -o 'gdb-remote localhost:1234' -o 'break set -H -r ".*pivot_root.*"' ~/Repos/linux/vmlinux
lldb -o 'gdb-remote localhost:1234' -o 'break set -H -f namespace.c -l 4197' ~/Repos/linux/vmlinux
