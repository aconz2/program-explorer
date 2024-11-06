#set -e

# https://raw.githubusercontent.com/cloud-hypervisor/cloud-hypervisor/master/vmm/src/api/openapi/cloud-hypervisor.yaml

k=/home/andrew/Repos/linux/vmlinux
#ch=$(realpath cloud-hypervisor-static)
ch=/home/andrew/Repos/cloud-hypervisor/target/debug/cloud-hypervisor

trap "pkill -P $$" EXIT KILL TERM

socket_path=/tmp/chapi.sock

rm -f ${socket_path}

# $ch \
#     --kernel $k \
#     --initramfs initramfs \
#     --serial off \
#     --cmdline "console=hvc0" \
#     --cpus boot=1 \
#     --memory size=1024M \
#     --event-monitor fd=2 \
#     -v \
#     --api-socket ${socket_path} > /tmp/ch.out &
$ch -v --api-socket ${socket_path} > /tmp/ch.out &

cat > /tmp/ch.config.json <<EOF
{
  "cpus": {
    "boot_vcpus": 1,
    "max_vcpus": 1
  },
  "memory": {
    "size": 1073741824
  },
  "payload": {
    "kernel": "/home/andrew/Repos/linux/vmlinux",
    "cmdline": "console=hvc0",
    "initramfs": "initramfs"
  },
  "pmem": [
      {
        "file": "ocismall.erofs",
        "discard_writes": true
      },
      {
        "file": "/tmp/perunner-io-file",
        "discard_writes": false
      }
  ],
  "serial": {
    "mode": "Off"
  },
  "console": {
    "mode": "Tty"
  }
}
EOF

curl --unix-socket ${socket_path} \
    -i -X PUT 'http://localhost/api/v1/vm.create' \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d '@/tmp/ch.config.json'

# curl --unix-socket ${socket_path} \
#     -i -X PUT 'http://localhost/api/v1/vm.add-pmem' \
#     -H 'Content-Type: application/json' \
#     -H 'Accept: application/json' \
#     -d '{"file": "ocismall.erofs", "discard_writes": true}'

#curl --unix-socket ${socket_path} \
#    -i -X PUT 'http://localhost/api/v1/vm.add-pmem' \
#    -H 'Content-Type: application/json' \
#    -H 'Accept: application/json' \
#    -d '{"file": "/tmp/perunner-io-file", "discard_writes": false}'

curl --unix-socket ${socket_path} \
    'http://localhost/api/v1/vm.info' \
    -H 'Accept: application/json' | jq

#cat /tmp/ch.out


sleep 1
#curl --unix-socket ${socket_path} \
#    -i -X PUT 'http://localhost/api/v1/vm.reboot'
#sleep 1
#wait


wait
# (./cloud-hypervisor-static -v --event-monitor path=/tmp/chevent --api-socket ${socket_path} | ts "%H:%M:%.S") > /tmp/chout 2> /tmp/cherr &
#
# config='{
#     "cpus": {"boot_vcpus": 1, "max_vcpus": 1},
#     "memory": {"size": 1073741824},
#     "payload": {"kernel": "/home/andrew/Repos/linux/vmlinux", "cmdline": "console=hvc0", "initramfs": "initramfs"},
#     "pmem": [{"file": "gcc-14.1.0.sqfs", "discard_writes": true}, {"file": "pmemtestfile"}],
#     "console": {"mode": "Tty"}
# }'
#
# time curl --unix-socket ${socket_path} -i \
#     -X PUT 'http://localhost/api/v1/vm.create' \
#      -H 'Accept: application/json'              \
#      -H 'Content-Type: application/json'        \
#      -d "${config}"
#
# echo 'pre  boot' | ts "%H:%M:%.S"
# time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.boot'
# echo 'post  boot' | ts "%H:%M:%.S"
# sleep 1
#
# echo 'rebooting'
#
# echo 'pre  reboot' | ts "%H:%M:%.S"
# time curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.reboot'
# echo 'post reboot' | ts "%H:%M:%.S"
# time curl --unix-socket ${socket_path} -X GET 'http://localhost/api/v1/vm.info'
# sleep 1
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

# wait
#
# cat /tmp/chout
# cat /tmp/cherr
