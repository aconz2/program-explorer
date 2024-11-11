package main

import (
    "encoding/json"
    "os"
    "net"
    "fmt"
    // "syscall"
    "math/rand"
    // "time"
    // "errors"
    "os/exec"
    // "context"
)
type StdoutWriter struct{}

func (StdoutWriter) Write(b []byte) (int, error) {
    return os.Stdout.Write(b)
}

type WorkerInput struct {
    File string `json:"file"`
}

type WorkerOutput struct {
    Status int `json:"status"`
}

func makeSockPath() string {
    return fmt.Sprintf("/tmp/sock-%x", rand.Int31())
}

func runWorker() (string, *exec.Cmd, *net.UnixListener, error) {
    sockPath := makeSockPath()

    conn, err := net.ListenUnix("unixpacket",  &net.UnixAddr{sockPath, "unixpacket"})
    if err != nil {
        return "", nil, nil, err
    }
    cmd := exec.Command("target/debug/peserver",
        "--socket", sockPath,
        "../ocismall.erofs",
    );
    cmd.Stderr = StdoutWriter{}
    cmd.Stdout = StdoutWriter{}
    err = cmd.Start()
    return sockPath, cmd, conn, err
}

func main() {
    sockPath, cmd, listener, err := runWorker()
    if err != nil {
        panic(err)
    }
    defer cmd.Wait()

    conn, err := listener.Accept()
    if err != nil {
        panic(err)
    }
    defer conn.Close()
    os.Remove(sockPath)

    _, err = conn.Write([]byte("{\"file\": \"/tmp/foo\"}"))
    if err != nil {
        panic(err)
    }

    data := make([]byte, 1024)
    n, err := conn.Read(data)
    if err != nil {
        panic(err)
    }
    output := WorkerOutput{}
    err = json.Unmarshal(data[0:n], &output)
    if err != nil {
        panic(err)
    }
    fmt.Println("got response from worker", output)
}

