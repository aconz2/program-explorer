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

type Worker struct {
    sockPath string
    cmd *exec.Cmd
    listener *net.UnixListener
    conn net.Conn
}

func (self *Worker) Cleanup() {
    self.conn.Close()
    self.cmd.Process.Kill()
    self.cmd.Wait()
}

func makeWorker() (*Worker, error) {
    ret := new(Worker)
    sockPath := makeSockPath()
    defer os.Remove(sockPath)

    listener, err := net.ListenUnix("unixpacket",  &net.UnixAddr{sockPath, "unixpacket"})
    if err != nil {
        return nil, err
    }
    cmd := exec.Command("target/debug/peserver",
        "--socket", sockPath,
        "../ocismall.erofs",
    );
    cmd.Stderr = StdoutWriter{}
    cmd.Stdout = StdoutWriter{}
    err = cmd.Start()
    if err != nil {
        return nil, err
    }

    conn, err := listener.Accept()
    if err != nil {
        return nil, err
    }

    ret.cmd = cmd
    ret.listener = listener
    ret.conn = conn

    return ret, nil
}

func (self *Worker) Read(data []byte) (int, error) {
    return self.conn.Read(data)
}

func (self *Worker) Write(data []byte) (int, error) {
    return self.conn.Write(data)
}

func main() {
    // i is for interactive
    // TODO add routes /api/v1/i/run
    //                 /api/v1/i/containers
    // /api/v1/i/run
    // grab a token from the pool for max requests
    // put body in tempfile
    // grab worker from pool and send it a message
    // wait for response, put worker back on pool
    // send response from tempfile
    // put token back to pool

    worker, err := makeWorker()
    defer worker.Cleanup()
    if err != nil {
        panic(err)
    }

    _, err = worker.Write([]byte("{\"file\": \"/tmp/foo\"}"))

    data := make([]byte, 1024)
    n, err := worker.Read(data)
    fmt.Println("did read")
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

func makeSockPath() string {
    return fmt.Sprintf("/tmp/sock-%x", rand.Int31())
}
