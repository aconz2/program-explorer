use std::io;
use std::io::{Read,Write};

use mio;
use mio::{Events, Interest, Poll, Registry};
use std::net::SocketAddr;
use mio::net::{TcpListener,TcpStream};
use std::time::Duration;
use clap::{Parser};
use tempfile::NamedTempFile;
use httparse;
use atoi_simd;

mod mytimerfd;
use mytimerfd::TimerFd;

const MAX_HEADER_SIZE_BYTES: usize = 4096;
const MAX_HEADER_COUNT: usize = 5;
// max body size caps the size of our io file, though we have the config header additionally and it
// will get truncated up to nearest 2 MB alignment, so really it should be about MAX_BODY_SIZE +
// 2MB since the config should always fit in 2 MB anyways
const MAX_BODY_SIZE: usize = 0xa00000; // 10 MB

struct Config {
    addr: SocketAddr,
    total_conn: usize,
    timeout_per_conn: Duration,
}

enum State {
    ReadingHeaders,
}

enum Token {
    Server,
    RequestConnection(usize),
    RequestTimer(usize),
}

impl Token {
    fn connection(index: usize) -> Self {
        Self::RequestConnection(index)
    }
    fn timer(index: usize) -> Self {
        Self::RequestTimer(index)
    }
}

impl From<mio::Token> for Token {
    fn from(mio::Token(x): mio::Token) -> Token {
        if x == 0 { return Token::Server; }
        let x = x - 1;
        let (index, k) = (x / 2, x % 2);
        match k {
            0 => Token::RequestConnection(index),
            1 => Token::RequestTimer(index),
            _ => unreachable!(),
        }
    }
}

impl Into<mio::Token> for Token {
    fn into(self) -> mio::Token {
        let i = match self {
            Token::Server => 0,
            Token::RequestConnection(i) => i * 2 + 1,
            Token::RequestTimer(i)      => i * 2 + 2,
        };
        mio::Token(i)
    }
}

struct RequestResources {
    index: usize,
    buf_offset: usize,
    buf: Box<[u8]>,
    timer: TimerFd,
    io_file: NamedTempFile,
}

impl RequestResources {
    fn new(index: usize) -> io::Result<Self> {
        let timer = TimerFd::new()?;
        Ok(Self {
            index: index,
            buf: Box::new([0; MAX_HEADER_SIZE_BYTES]),
            buf_offset: 0,
            timer: timer,
            io_file: NamedTempFile::new()?,
        })
    }

    fn into_request(self, conn_addr: (TcpStream, SocketAddr)) -> Request {
        Request::new(self, conn_addr)
    }

    fn reset(mut self) -> Self {
        self.buf_offset = 0;
        self.timer.unset().expect("error resetting timer"); // idk propagate?
        // self.buf.clear();
        self.io_file.as_file().set_len(0).expect("error truncating");
        self
    }
}

struct Request {
    state: State,
    resources: RequestResources,
    address: SocketAddr,
    connection: TcpStream,
}

fn escape_dump(input: &[u8]) {
    use std::io::Write;
    let mut output = Vec::<u8>::with_capacity(1024);
    for b in input {
        for e in std::ascii::escape_default(*b) {
            output.push(e);
        }
    }
    let _ = io::stdout().write_all(output.as_slice()).unwrap();
    println!("");
}

impl Request {
    fn new(resources: RequestResources, (connection, address): (TcpStream, SocketAddr)) -> Self {
        Self {
            state: State::ReadingHeaders,
            resources: resources,
            address: address,
            connection: connection,
        }
    }

    fn index(&self) -> usize { self.resources.index }

    fn into_resources(mut self, registry: &Registry) -> RequestResources {
        let _ = self.deregister(registry);
        self.resources.reset()
    }

    fn connection_ready(&mut self) -> io::Result<()> {
        // we are going to have to transition to registering for writable, so we need the
        // registry here
        match self.state {
            State::ReadingHeaders => {
                if self.resources.buf_offset + 1 == self.resources.buf.len() {
                    panic!("guaranteed by check in partial read");
                }
                let n = self.connection.read(&mut self.resources.buf[self.resources.buf_offset..])?;
                if n == 0 {
                    todo!("eof, send error response and close");
                }
                let data = &self.resources.buf[..self.resources.buf_offset + n];
                self.resources.buf_offset = data.len();
                {
                    escape_dump(&data);
                }
                // I tried for a second to move these into RequestResources but the lifetimes get a
                // bit hairy
                let mut headers = [httparse::EMPTY_HEADER; MAX_HEADER_COUNT];
                let mut request = httparse::Request::new(&mut headers[..]);
                let res = request.parse(data);
                match res {
                    Ok(httparse::Status::Complete(body_start)) => {
                        eprintln!("yo lets go method={:?} path={:?} version={:?}", request.method, request.path, request.version);
                        let body = &data[body_start..];
                        match content_length(&request) {
                            Some(body_length) => {
                                if body_length as usize > MAX_BODY_SIZE {
                                    todo!("body size too big, send error response and close");
                                } else if body.len() == body_length as usize {
                                    todo!("complete body");
                                    // tiny write, just do it
                                    match self.resources.io_file.write_all(body) {
                                        Ok(_) => {
                                            todo!("ready to put me on the queue");
                                        },
                                        Err(_) => {
                                            todo!("got error writing body");
                                        }
                                    }
                                } else {
                                    todo!("incomplete body, move to ReadingBody state");
                                }
                            },
                            None => {
                                todo!("no content length, send error and close");
                            }
                        }
                    }
                    Ok(httparse::Status::Partial) => {
                        if self.resources.buf_offset == MAX_HEADER_SIZE_BYTES {
                            todo!("max header size exceeded, send error response and close");
                        }
                        eprintln!("not enough request data yet");
                    }
                    Err(e) => {
                        todo!("malformed request, send error and close");
                    }
                }

            }
        }
        Ok(())
    }

    fn register(&mut self, registry: &Registry) -> io::Result<()> {
        let token_conn = Token::connection(self.index());
        //let token_time = Token::timer(self.index());
        registry.register(&mut self.connection, token_conn.into(), Interest::READABLE)?;
        //registry.register(&mut self.resources.timer, token_time.into(), Interest::READABLE)?;
        Ok(())
    }

    fn deregister(&mut self, registry: &Registry) {
        let _ = registry.deregister(&mut self.connection);
    }
}


struct RequestPool {
    active: Vec<Option<Request>>,
    inactive: Vec<RequestResources>,
}

impl RequestPool {
    fn new(total_conn: usize) -> io::Result<Self> {
        let mut ret = Self {
            active: std::iter::repeat_with(|| None).take(total_conn).collect(),
            inactive: Vec::with_capacity(total_conn),
        };
        for i in 0..total_conn {
            ret.inactive.push(RequestResources::new(i)?);
        }
        Ok(ret)
    }

    fn pop(&mut self) -> Option<RequestResources> {
        self.inactive.pop()
    }

    fn activate(&mut self, mut req: Request, registry: &Registry) -> io::Result<()> {
        let index = req.index();
        if self.active[index].is_some() {
            panic!("no free slot here {index}");
        }
        req.register(registry)?;
        self.active[index] = Some(req);
        Ok(())
    }

    fn deactivate(&mut self, index: usize, registry: &Registry) {
        if self.active[index].is_none() {
            panic!("no active request here {index}");
        }
        let resources = self.active[index].take().unwrap().into_resources(registry);
        self.inactive.push(resources);
        // TODO unregister from registry?
    }

    fn connection_ready(&mut self, index: usize) -> io::Result<()> {
        match self.active[index] {
            None => { panic!("no active request here {index}"); },
            Some(ref mut req) => {
                req.connection_ready()
            }
        }
    }
}

fn serve(config: Config) -> io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(128);
    const SERVER: mio::Token = mio::Token(0);

    let mut listener = TcpListener::bind(config.addr)?;
    poll.registry().register(&mut listener, SERVER, Interest::READABLE)?;

    let mut reqpool = RequestPool::new(config.total_conn)?;

    loop {
        poll.poll(&mut events, Some(Duration::from_millis(100)))?;

        for event in events.iter() {
            match event.token().into() {
                Token::Server => loop {
                    match reqpool.pop() {
                        Some(resources) => {
                            match listener.accept() {
                                Ok(conn_addr) => {
                                    // eprintln!("Got a connection from: {}", address);
                                    reqpool.activate(resources.into_request(conn_addr), poll.registry())
                                        .expect("couldnt register");
                                },
                                // A "would block error" is returned if the operation
                                // is not ready, so we'll stop trying to accept
                                // connections.
                                Err(ref err) if would_block(err) => break,
                                Err(err) => return Err(err),
                            }
                        }
                        None => {
                            eprintln!("backup, someone is knocking but not ready to answer");
                            break;
                        }
                    }
                },
                Token::RequestConnection(i) => {
                    match reqpool.connection_ready(i) {
                        Ok(_) => {},
                        Err(e) => {
                            eprintln!("error on connection {e}");
                            todo!();
                        }
                    }
                },
                _ => todo!()
            }
        }
    }
}

fn would_block(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8000")]
    addr: String,
}

fn main() {
    let args = Args::parse();
    let addr = args.addr.parse().expect("bad socket addr");
    let config = Config {
        addr: addr,
        total_conn: 4,
        timeout_per_conn: Duration::from_millis(1000 * 5),
    };
    serve(config).unwrap();
}

#[cfg(test)]
mod tests {
    use httparse;
    use super::*;

    #[test]
    fn test_request_reuse() {
        let mut headers = [httparse::EMPTY_HEADER; 5];
        {
            let mut req = httparse::Request::new(&mut headers[..]);
            let _ = req.parse(b"GET / HTTP/1.1\r\nHeader1: value1\r\nContent-Length: 123\r\n\r\n").unwrap();
            assert_eq!(content_length(&req), Some(123));
        }
        {
            let mut req = httparse::Request::new(&mut headers[..]);
            let _ = req.parse(b"GET / HTTP/1.1\r\nContent-Length: 123\r\nHeader1: value1\r\n\r\n").unwrap();
            assert_eq!(content_length(&req), Some(123));
        }
        {
            let mut req = httparse::Request::new(&mut headers[..]);
            let _ = req.parse(b"GET / HTTP/1.1\r\nHeader1: value1\r\n\r\n").unwrap();
            assert_eq!(content_length(&req), None);
        }
        {
            let mut req = httparse::Request::new(&mut headers[..]);
            let _ = req.parse(b"GET / HTTP/1.1\r\nContenT-LENGTH: kdjkfj\r\n\r\n").unwrap();
            assert_eq!(content_length(&req), None);
        }
        {
            let mut req = httparse::Request::new(&mut headers[..]);
            let _ = req.parse(b"GET / HTTP/1.1\r\nContenT-LENGTH: 999999999999999999999999999999\r\n\r\n").unwrap();
            assert_eq!(content_length(&req), None);
        }
    }
}

fn content_length(req: &httparse::Request) -> Option<u64> {
    for header in req.headers.iter() {
        if header.name.eq_ignore_ascii_case("content-length") {  // TODO could do this simd
            return atoi_simd::parse::<u64>(header.value).ok()
        }
    }
    None
}
