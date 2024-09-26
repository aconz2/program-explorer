#!/usr/bin/python
#
# mallocstacks  Trace malloc() calls in a process and print the full
#               stack trace for all callsites.
#               For Linux, uses BCC, eBPF. Embedded C.
#
# This script is a basic example of the new Linux 4.6+ BPF_STACK_TRACE
# table API.
#
# Copyright 2016 GitHub, Inc.
# Licensed under the Apache License, Version 2.0 (the "License")

# needs dnf install bcc kernel-module-core

import os
import sys
import signal

from bcc import BPF
from bcc.utils import printb
from time import sleep

# if len(sys.argv) < 2:
#     print("USAGE: mallocstacks PID [NUM_STACKS=1024]")
#     exit()
# pid = int(sys.argv[1])
# if len(sys.argv) == 3:
#     try:
#         assert int(sys.argv[2]) > 0, ""
#     except (ValueError, AssertionError) as e:
#         print("USAGE: mallocstacks PID [NUM_STACKS=1024]")
#         print("NUM_STACKS must be a non-zero, positive integer")
#         exit()
#     stacks = sys.argv[2]
# else:
stacks = 1024

# pid = os.fork()
# if pid == 0:  # child
#     print('child', os.getpid())
#     os.kill(os.getpid(), signal.SIGSTOP)
#     os.execvp(sys.argv[1], sys.argv[1:])

# TODO why doesn't it pick this up
os.environ['BCC_KERNEL_SOURCE'] = '/lib/modules/6.10.11-200.fc40.x86_64/source'

# load BPF program
b = BPF(text=f"""
#include <uapi/linux/ptrace.h>

BPF_HASH(calls, int);
BPF_STACK_TRACE(stack_traces, {stacks});

int alloc_enter(struct pt_regs *ctx, size_t size) {{
    int key = stack_traces.get_stackid(ctx, BPF_F_USER_STACK);
    if (key < 0)
        return 0;

    // could also use `calls.increment(key, size);`
    u64 zero = 0, *val;
    val = calls.lookup_or_try_init(&key, &zero);
    if (val) {{
      (*val) += size;
    }}
    return 0;
}};
""", debug=0)

b.attach_uprobe(name="c", sym="malloc", fn_name="alloc_enter", pid=pid)
print("Attaching to malloc in pid %d, Ctrl+C to quit." % pid)

os.kill(pid, signal.SIGCONT)

# sleep until Ctrl-C
try:
    sleep(99999999)
except KeyboardInterrupt:
    pass

calls = b.get_table("calls")
stack_traces = b.get_table("stack_traces")

for k, v in reversed(sorted(calls.items(), key=lambda c: c[1].value)):
    print("%d bytes allocated at:" % v.value)
    if k.value > 0 :
        for addr in stack_traces.walk(k.value):
            printb(b"\t%s" % b.sym(addr, pid, show_offset=True))
        print("    %d\n" % v.value)
