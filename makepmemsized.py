import sys
from pathlib import Path

alignment = 0x20_0000

filename = sys.argv[1]
file = Path(filename)

size = file.stat().st_size

if size % alignment == 0:
    print(f'Size {size} is already aligned')
    sys.exit(0)

remainder = size % alignment
extra = alignment - remainder
assert (size + extra) % alignment == 0

with open(file, 'ab') as fh:
    fh.write(b'\x00' * extra)

new_size = file.stat().st_size
assert new_size % alignment == 0
print(f'Size {new_size} now aligned')


# TODO maybe look at using truncate to do this for sparse file, but prolly doesn't matter
