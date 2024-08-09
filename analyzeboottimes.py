import sys

# the linux/scripts/bootgraph.pl script says kernel params initcall_debug printk.time=1
# perl ~/Repos/linux/scripts/bootgraph.pl < boottimes 

prev_time = None
prev_event = None

stats = []

with open(sys.argv[1]) as fh:
    for line in fh:
        if line.startswith('['):
            i_end = line.find(']')
            time = float(line[1:i_end].strip())
            event = line[i_end+1:-1]
            if prev_time is None:
                prev_time = time
                prev_event = event
            else:
                duration = time - prev_time
                stats.append((duration, prev_event))
                prev_time = time
                prev_event = event

stats.sort(reverse=True)
for duration, event in stats:
    print('{:8.2f} ms {}'.format(duration * 1000, event))
