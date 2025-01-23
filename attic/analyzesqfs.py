from PySquashfsImage import SquashFsImage
import sys
import hashlib

def file_hash(f):
    hasher = hashlib.new('sha256')
    for block in f.iter_bytes():
        hasher.update(block)
    return hasher.digest()

def file_hashes(im):
    d = {}
    dir_count = 0
    for item in im:
        if item.is_file:
            d[file_hash(item)] = item.size
        elif item.is_dir:
            dir_count += 1

    return dir_count, d

p1 = sys.argv[1]
p2 = sys.argv[2]

im1 = SquashFsImage.from_file(p1)
im2 = SquashFsImage.from_file(p2)

dc1, h1 = file_hashes(im1)
dc2, h2 = file_hashes(im2)

fsize1 = sum(h1.values())
fsize2 = sum(h2.values())

shared = set(h1) & set(h2)
shared_size = sum(h1[k] for k in shared)

print('{:10} {:10.2f} Mb (compressed) {:10.2f} Mb (uncompressed) {:10} files {:10} dirs'.format(p1, im1.size / 1e6, fsize1 / 1e6, len(h1), dc1))
print('{:10} {:10.2f} Mb (compressed) {:10.2f} Mb (uncompressed) {:10} files {:10} dirs'.format(p2, im2.size / 1e6, fsize2 / 1e6, len(h2), dc2))

print('{} {:10.2f} Mb shared {:5.2f}%'.format(len(shared), shared_size / 1e6, shared_size / max(fsize1, fsize2) * 100))
