set -e

function get_layers() {
    image_manifest=$(jq -r '.manifests[0].digest' index.json | sed 's_:_/_')
    jq -r '.layers[].digest' blobs/$image_manifest | sed 's_:_/_'
}

function show_layers() {
    pushd . &>/dev/null
    cd "$1"
    for layer in $(get_layers | tail -n+2); do
        echo $layer
        tar tvf blobs/$layer
        echo
    done
    popd &>/dev/null
}

mkdir -p /tmp/question
cd /tmp/question
name=githubquestion

# use Dockerfile name b/c I don't know how to get buildkit to use a different name
cat << "EOF" > Dockerfile
FROM docker.io/library/busybox
RUN echo hi > a
RUN echo hi > b
EOF

trap 'trap - SIGTERM && kill 0' SIGINT SIGTERM EXIT

# NOTE: we have to remove the image between builds otherwise it will use the build cache
# even though we use --dns=none --no-hosts --no-hostname the second time around

echo "# podman build -f Dockerfile"
podman rmi --ignore $name >/dev/null
podman build -f Dockerfile -t $name >/dev/null
rm -rf oci-dir && mkdir oci-dir
podman save --format oci-archive $name | tar xf - -C oci-dir
show_layers oci-dir

echo -e "---------------------------------\n"

echo "# podman build --dns=none --no-hosts --no-hostname -f Dockerfile"
podman rmi $name >/dev/null
podman build --dns=none --no-hosts --no-hostname -f Dockerfile -t $name >/dev/null
rm -rf oci-dir && mkdir oci-dir
podman save --format oci-archive $name | tar xf - -C oci-dir
show_layers oci-dir

echo -e "---------------------------------\n"

mkdir -p varrun
trap 'kill $(jobs -p)' EXIT

echo "# docker build . (containerized docker)"
podman run --privileged --rm -v $(realpath varrun):/var/run/ docker:latest &>/dev/null &
sleep 2  # wait for daemon to load
podman run --privileged --rm -v $(realpath varrun):/var/run -v $(realpath .):/$(realpath .) -w $(realpath .) docker:latest build -t $name . &>/dev/null
rm -rf oci-dir && mkdir oci-dir
podman run --privileged --rm -v $(realpath varrun):/var/run -v $(realpath .):/$(realpath .) -w $(realpath .) docker:latest save $name | tar xf - -C oci-dir
show_layers oci-dir
kill %%

echo -e "---------------------------------\n"

# varrun (from above) has root owned files, docker fails when trying to load them as context
mkdir -p clean
cp Dockerfile clean/
cd clean

# NOTE: for this I have an externally running command like
# wget https://download.docker.com/linux/static/stable/x86_64/docker-27.3.1.tgz
# tar xf docker-27.3.1.tgz  # unpacks a docker dir
# wget https://github.com/moby/buildkit/releases/download/v0.17.0/buildkit-v0.17.0.linux-amd64.tar.gz
# tar xf buildkit-v0.17.0.linux-amd64.tar.gz  # unpacks a bin dir
# PATH=$(realpath docker):$PATH sudo --preserve-env=PATH dockerd
# PATH=$(realpath bin):$PATH    sudo --preserve-env=PATH buildkitd
# sudo chown $USER:$USER /var/run/docker.sock
# sudo chown $USER:$USER /var/run/buildkit/buildkitd.sock

echo "# docker build . (non-containerized docker)"
# tried to get this to work but to no avail
#sudo --preserve-env=PATH dockerd &
#sudo chown $USER:$USER /var/run/docker.sock
docker build -f Dockerfile -t $name . &>/dev/null
rm -rf oci-dir && mkdir oci-dir
docker save $name | tar xf - -C oci-dir
show_layers oci-dir

echo -e "---------------------------------\n"

echo "# buildctl build --frontend dockerfile.v0 --local dockerfile=. (non-containerized buildkit)"
# podman run --privileged --rm docker.io/moby/buildkit:latest &  # tried getting this to work but no avail
rm -rf oci-dir && mkdir oci-dir
#buildctl --addr=podman-container://buildkitd build --frontend dockerfile.v0 --local context=. --local dockerfile=. --output type=oci | tar xf - -C oci-dir
buildctl build --frontend dockerfile.v0 --local dockerfile=. --output type=oci 2>/dev/null | tar xf - -C oci-dir
show_layers oci-dir

