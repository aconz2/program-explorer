set -e

version=14.1.0
#version=13.3.0
#sqfstar=sqfstar
sqfstar=~/Repos/squashfs-tools/squashfs-tools/sqfstar
outfile=gcc-${version}.sqfs

rm -f $outfile

id=$(podman create docker.io/library/gcc:${version})
trap "podman rm $id" EXIT

podman export "$id" | $sqfstar -uid-gid-offset 1000 -comp zstd $outfile

python makepmemsized.py $outfile
