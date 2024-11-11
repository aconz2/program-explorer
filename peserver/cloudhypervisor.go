package main

type ConsoleConfig struct {
    File string `json:"file"`
    Mode string `json:"mode"`
}

type CpusConfig struct {
    Boot_vcpus int `json:"boot_vcpus"`
    Max_vcpus  int `json:"max_vcpus"`
}

type PayloadConfig struct {
    Kernel    string `json:"kernel"`
    Cmdline   string `json:"cmdline"`
    Initramfs string `json:"initramfs"`
}

type MemoryConfig struct {
    Size int `json:"size"`
}

type PmemConfig struct {
    File             string `json:"file"`
    Discard_writes   bool   `json:"discard_writes"`
}

type VmConfig struct {
    Cpus     CpusConfig    `json:"cpus"`
    Memory   MemoryConfig  `json:"memory"`
    Payload  PayloadConfig `json:"payload"`
    Pmem     []PmemConfig  `json:"pmem"`
    Console  ConsoleConfig `json:"console"`
}


func testJsonBlob() []byte {
    return []byte(`
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
`)
}

    // var vmConfig VmConfig
    // err := json.Unmarshal(testJsonBlob(), &vmConfig);
    // if err != nil {
    //     fmt.Println("error: ", err)
    // } else {
    //     fmt.Printf("%#v\n", vmConfig)
    // }
