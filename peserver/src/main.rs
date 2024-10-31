use std::io;
use std::io::{Read};

use mio;
use mio::{Events, Interest, Poll, Registry};
use std::net::SocketAddr;
use mio::net::{TcpListener,TcpStream};
use std::time::Duration;
use clap::{Parser};
use tempfile::NamedTempFile;
use httparse;

mod mytimerfd;
use mytimerfd::TimerFd;

// use nix::sys::timerfd::{TimerFd,ClockId,TimerFlags};

const MAX_HEADER_SIZE_BYTES: usize = 4096;

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

    fn into_resources(self) -> RequestResources { self.resources.reset() }

    fn connection_ready(&mut self) -> io::Result<()> {
        match self.state {
            State::ReadingHeaders => {
                let n = self.connection.read(&mut self.resources.buf[self.resources.buf_offset..])?;
                if n == 0 {
                    todo!();
                }
                // if n is 0, end of connection
                let data = &self.resources.buf[..self.resources.buf_offset + n];
                self.resources.buf_offset = data.len();
                {
                    escape_dump(&data);
                }
                let mut headers = [httparse::EMPTY_HEADER; 5];
                let mut request = httparse::Request::new(&mut headers[..]);
                let res = request.parse(data);
                match res {
                    Ok(httparse::Status::Complete(body_start)) => {
                        eprintln!("yo lets go method={:?} path={:?} version={:?}", request.method, request.path, request.version);
                        eprintln!("{body_start} {}", data.len());
                        let body = &data[body_start..];
                        if !body.is_empty() {
                            let len = body.len();
                            eprintln!("yo got some body {len}");
                            escape_dump(&body);
                        }
                    }
                    Ok(httparse::Status::Partial) => {
                        eprintln!("not enough request data yet");
                    }
                    Err(e) => {
                        eprintln!("oh no {e}");
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
        let index = req.resources.index;
        if self.active[index].is_some() {
            panic!("no free slot here");
        }
        req.register(registry)?;
        self.active[index] = Some(req);
        Ok(())
    }

    fn deactivate(&mut self, index: usize) {
        if self.active[index].is_none() {
            panic!("no active request here {index}");
        }
        let resources = self.active[index].take().unwrap().into_resources();
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
            // We can use the token we previously provided to `register` to
            // determine for which type the event is.
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
