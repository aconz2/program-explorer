socket_path=/tmp/chapi.sock

config='{
    "cpus": {"boot_vcpus": 1, "max_vcpus": 1},
    "memory": {"size": 1073741824},
    "payload": {"kernel": "/home/andrew/Repos/linux/vmlinux", "cmdline": "console=hvc0", "initramfs": "initramfs"},
    "pmem": [{"file": "gcc-14.1.0.sqfs", "discard_writes": true}, {"file": "pmemtestfile"}],
    "console": {"mode": "Off"}
}'

curl --unix-socket ${socket_path} -i \
    -X PUT 'http://localhost/api/v1/vm.create' \
     -H 'Accept: application/json'              \
     -H 'Content-Type: application/json'        \
     -d "${config}"

curl --unix-socket ${socket_path} -i -X PUT 'http://localhost/api/v1/vm.boot'
