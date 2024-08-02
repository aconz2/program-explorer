id=$(podman create docker.io/library/gcc:14.1.0)
podman export "$id" | sqfstar -comp zstd gcc-14.sqfs
podman rm "$id"
