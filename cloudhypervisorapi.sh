#set -e

# https://raw.githubusercontent.com/cloud-hypervisor/cloud-hypervisor/master/vmm/src/api/openapi/cloud-hypervisor.yaml

k=/home/andrew/Repos/linux/vmlinux

trap "pkill -P $$" EXIT KILL TERM

socket_path=/tmp/chapi.sock

rm -f ${socket_path}

(./cloud-hypervisor-static --api-socket ${socket_path} | ts "%H:%M:%.S") > /tmp/chout 2> /tmp/cherr &

config='{
    "cpus": {"boot_vcpus": 1, "max_vcpus": 1},
    "memory": {"size": 1073741824},
    "payload": {"kernel": "/home/andrew/Repos/linux/vmlinux", "cmdline": "console=hvc0", "initramfs": "initramfs"},
    "pmem": [{"file": "gcc-14.1.0.sqfs", "discard_writes": true}, {"file": "pmemtestfile"}],
    "console": {"mode": "Tty"}
}'

time curl --unix-socket ${socket_path} -i \
    -X PUT 'http://localhost/api/v1/vm.create' \
     -H 'Accept: application/json'              \
     -H 'Content-Type: application/json'        \
     -d "${config}"

echo 'pre  boot' | ts "%H:%M:%.S"
time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.boot'
echo 'post  boot' | ts "%H:%M:%.S"
sleep 1

echo 'rebooting'

echo 'pre  reboot' | ts "%H:%M:%.S"
time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.reboot'
echo 'post reboot' | ts "%H:%M:%.S"
time curl --unix-socket ${socket_path} -X GET 'http://localhost/api/v1/vm.info'
sleep 1
pkill -P $$
# time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.shutdown'
# time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.delete'

#sleep 1
#time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.shutdown'

#time curl --unix-socket ${socket_path} -i \
#    -X PUT 'http://localhost/api/v1/vm.create' \
#     -H 'Accept: application/json'              \
#     -H 'Content-Type: application/json'        \
#     -d "${config}"
#
#time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.boot'
#sleep 2
#time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.boot'
#time curl --unix-socket ${socket_path} -X GET 'http://localhost/api/v1/vm.info' | jq

wait

cat /tmp/chout
cat /tmp/cherr
