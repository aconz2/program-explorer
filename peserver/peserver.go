package main

import (
    "encoding/json"
    "encoding/binary"
    "os"
    "net"
    "net/http"
    "fmt"
    "log"
    "io"
    "math/rand"
    "time"
    "os/exec"
    "os/signal"
    "syscall"
)

// NOTE: currently must fit in u32
const MaxFileSize = 0x20_0000 // 2 MB

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
    buf []byte
}

func (self *Worker) Cleanup() {
    self.conn.Close()
    self.cmd.Process.Kill()
    self.cmd.Wait()
}

func makeWorker(id int) (*Worker, error) {
    ret := new(Worker)
    sockPath := fmt.Sprintf("/tmp/sock-%x", rand.Int31())
    defer os.Remove(sockPath)

    listener, err := net.ListenUnix("unixpacket",  &net.UnixAddr{sockPath, "unixpacket"})
    if err != nil {
        return nil, err
    }
    cmd := exec.Command("target/debug/peserver",
        "--id", fmt.Sprintf("%d", id),
        "--socket", sockPath,
        "../ocismall.erofs",
    );
    cmd.WaitDelay = 100 * time.Millisecond
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
    ret.buf = make([]byte, 1024)

    return ret, nil
}

func (self *Worker) Read(data []byte) (int, error) {
    return self.conn.Read(data)
}

func (self *Worker) Write(data []byte) (int, error) {
    return self.conn.Write(data)
}

func (self *Worker) Run(input WorkerInput) (*WorkerOutput, error) {
    buf, err := json.Marshal(input); if err != nil {
        return nil, err
    }
    if _, err = self.Write(buf); err != nil {
        return nil, err
    }
    n, err := self.Read(self.buf); if err != nil {
        return nil, err
    }
    ret := new(WorkerOutput)
    if err = json.Unmarshal(self.buf[0:n], ret); err != nil {
        return nil, err
    }
    return ret, nil
}

type v1iRunner struct {
    files chan *os.File
    workers chan *Worker
}

func sendError(status int, w http.ResponseWriter, message string) {
    w.WriteHeader(status)
    w.Header().Set("content-type", "application/json")
    if len(message) > 0 {
        io.WriteString(w, message)
    }
}

func (self *v1iRunner) Shutdown() {
    close(self.files)
    close(self.workers)
    log.Println("cleaning files")
    for file := range self.files {
        os.Remove(file.Name())
    }
    log.Println("closing workers")
    for worker := range self.workers {
        worker.Cleanup()
    }
}

func (self *v1iRunner) ServeHTTP(w http.ResponseWriter, r *http.Request) {
    if r.ContentLength == -1 {
        log.Println("missing content length")
        sendError(http.StatusBadRequest, w, `{"error": "missing content length"}`)
        return
    }
    var file *os.File
    select {
    case file = <- self.files:
        // noop
    case <- time.After(time.Second):
        log.Println("not enough tokens in wait time")
        sendError(http.StatusServiceUnavailable, w, "")
        return
    }
    defer func() { self.files <- file }()

    // save body into file
    err := file.Truncate(0)
    if err != nil {
        sendError(http.StatusInternalServerError, w, `{"error": "err truncating file"}`)
        return
    }
    if err = binary.Write(file, binary.LittleEndian, uint32(r.ContentLength)); err != nil {
        sendError(http.StatusInternalServerError, w, `{"error": "err writing content length"}`)
        return
    }
    n, err := io.Copy(file, http.MaxBytesReader(w, r.Body, MaxFileSize));
    if err != nil {
        sendError(http.StatusInternalServerError, w, `{"error": "err copying body"}`)
        return
    }
    log.Println("body-size=", n)

    // grab worker and run
    worker := <- self.workers
    defer func() { self.workers <- worker }()

    // TODO should we be sending an fd? we still need it visible on the fs
    // for cloud-hypervisor at the moment (custom frontend would solve that)
    workerInput := WorkerInput { File: file.Name() }
    workerOutput, err := worker.Run(workerInput)
    if err != nil {
        sendError(http.StatusInternalServerError, w, `{"error": "todo"}`)
        return
    }

    // BAD: this doesn't release the worker promptly!
    // we really have a pool of tempfiles that are incoming and outgoing
    // and we shouldn't have the worker tied up for either

    // send response, read u32 content length from file
    w.WriteHeader(workerOutput.Status)
    // w.Header().Set("content-type", "application/json") // TODO this will be something custom I think
    _, err = file.Seek(0, 0)
    if err != nil {
        sendError(http.StatusInternalServerError, w, `{"error": "err seeking"}`)
        return
    }
    var contentLength uint32
    if err = binary.Read(file, binary.LittleEndian, &contentLength); err != nil {
        sendError(http.StatusInternalServerError, w, `{"error": "err reading content length"}`)
        return
    }
    if _, err = io.CopyN(w, file, int64(contentLength)); err != nil {
        log.Println("err writing response")
    }
}

func exitSignal(c chan os.Signal, runner *v1iRunner) {
    <- c
    runner.Shutdown()
    os.Exit(0)
}

func main() {
    // TODO  /api/v1/i/containers
    // where are we getting this data from?
    tokenCapacity := 10
    numWorkers := 3
    runner := new(v1iRunner)
    runner.files = make(chan *os.File, tokenCapacity)
    for i := 0; i < tokenCapacity; i++ {
        f, err := os.CreateTemp("/tmp", "peio"); if err != nil {
            log.Fatal("creating temp file ", err)
        }
        runner.files <- f
    }
    runner.workers = make(chan *Worker, numWorkers)
    for i := 0; i < numWorkers; i++ {
        worker, err := makeWorker(i); if err != nil {
            log.Fatal("making worker ", err)
        }
        runner.workers <- worker
    }

    http.Handle("POST /api/v1/i/run", runner)

    exitSignalChan := make(chan os.Signal, 1)
    signal.Notify(exitSignalChan, syscall.SIGINT, syscall.SIGTERM, syscall.SIGABRT)
    go exitSignal(exitSignalChan, runner)

    s := &http.Server{
        Addr:              ":8080",
        // Handler:        myHandler,
        ReadTimeout:       10 * time.Second,
        ReadHeaderTimeout:  2 * time.Second,
        WriteTimeout:      10 * time.Second,
        IdleTimeout:       10 * time.Second,
        MaxHeaderBytes:    1 << 20,
    }
    log.Fatal(s.ListenAndServe())
}

func testWorker() {
    worker, err := makeWorker(0)
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
