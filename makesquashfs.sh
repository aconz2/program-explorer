version=14.1.0
#version=13.3.0
id=$(podman create docker.io/library/gcc:${version})
podman export "$id" | sqfstar -comp zstd gcc-${version}.sqfs
podman rm "$id"
