import sys
from functools import partial
import string

pids = {}

def pid_letter(pid):
    if pid in pids:
        return pids[pid]
    pids[pid] = string.ascii_uppercase[len(pids)]
    return pids[pid]

def xform(line, t0=0):
    pid, time, msg = line.split(' ', maxsplit=2)
    t = float(time)
    p = pid_letter(pid)
    t_off = (t - t0) * 1000
    return f'{t_off: 8.2f}  {p}  {msg}'

f = sys.argv[1]

with open(f, 'r') as fh:
    lines = list(fh)

t0 = float(lines[0].split(' ', maxsplit=2)[1])

out = map(partial(xform, t0=t0), lines)

with open(f, 'w') as fh:
    fh.write(''.join(out))
