import sys
import json
with open(sys.argv[1]) as fh:
    j = json.load(fh)

out = []
for group, events in j['events'].items():
    for event in events:
        name = event['event']
        duration = event['end_timestamp']['nanos'] - event['timestamp']['nanos']
        key = f'{group} {name}'
        out.append((key, duration / 1000 / 1000))

out.sort(key=lambda x: x[1])
for k, v in out:
    print(f'{k:40s} {v:0.2f}ms')
