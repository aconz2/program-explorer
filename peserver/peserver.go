package main

import (
    "encoding/json"
    "fmt"
    // "math/rand"
    "time"
    "errors"
    "os/exec"
    "context"
)

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

func runCloudHypervisor() {
    // rng := rand.Int31()
    // socketPath := fmt.Sprintf("/tmp/ch.sock-%x", rng)
	ctx, cancel := context.WithTimeout(context.Background(), 1000*time.Millisecond)
	defer cancel()

    cmd := exec.CommandContext(ctx, "../cloud-hypervisor-static")
    cmd.WaitDelay = 10 * time.Millisecond
    cmd.Args = append(cmd.Args, []string{
        "--kernel", "/home/andrew/Repos/program-explorer/vmlinux",
        "--initramfs", "/home/andrew/Repos/program-explorer/initramfs",
        "--cpus", "boot=1",
        "--memory", "size=1024M",
        "--cmdline", "console=hvc0",
        "--pmem", "file=/home/andrew/Repos/program-explorer/ocismall.erofs,discard_writes=true file=/tmp/perunner-io-file,discard_writes=false",
    }...)
    out, err := cmd.Output()
    exitError := &exec.ExitError{}
    if errors.As(err, &exitError) {
        exitCode := exitError.ExitCode()
        fmt.Println("got exit error", exitError)
        fmt.Println("exit code", exitCode)
        fmt.Println("stderr:", string(exitError.Stderr[:]))
    }
    fmt.Println(string(out[:]))
    // if err != nil {
    //     fmt.Println("hit timeout running cloud hypervisor or maybe?", err)
    //     fmt.Println(ctx.Err()) // this will be context.Canceled if canceled
    // } else {
    //     fmt.Println(out)
    // }
    // if err := cmd.Run(); err != nil {
    //     fmt.Println("hit timeout running cloud hypervisor or maybe?", err)
    // }
}

func main() {
    // if len(os.Args) == 1 {
    //     fmt.Fprintf(os.Stderr, "  ...\n");
    //     os.Exit(1)

    // }
    var vmConfig VmConfig
    err := json.Unmarshal(testJsonBlob(), &vmConfig);
    if err != nil {
        fmt.Println("error: ", err)
    } else {
        fmt.Printf("%#v\n", vmConfig)
    }
    runCloudHypervisor()

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

