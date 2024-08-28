import sys
import os

alignment = 0x20_0000

filename = sys.argv[1]
fd = os.open(filename, os.O_RDWR)
assert fd > 0

size = os.fstat(fd).st_size

if size % alignment == 0:
    print(f'Size {size} is already aligned')
    sys.exit(0)

remainder = size % alignment
extra = alignment - remainder
new_size = size + extra
assert new_size % alignment == 0

os.ftruncate(fd, new_size)

new_size = os.fstat(fd).st_size
assert new_size % alignment == 0
print(f'Size {new_size} now aligned')
