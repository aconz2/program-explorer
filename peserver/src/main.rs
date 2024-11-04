use std::io;
use std::io::{ErrorKind};
//use std::io::{Read,Write};
use std::io::Cursor;

use tokio::io::{AsyncReadExt,AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

//use mio;
//use mio::{Events, Interest, Poll, Registry};
use std::net::SocketAddr;
//use mio::net::{TcpListener,TcpStream};
use std::time::Duration;
use clap::{Parser};
//use tempfile::NamedTempFile;
use httparse;
use atoi_simd;
use byteorder::{ReadBytesExt,LE};
use serde::{Serialize,Deserialize};

// mod mytimerfd;
// use mytimerfd::TimerFd;

const MAX_HEADER_SIZE_BYTES: usize = 4096;
const MAX_HEADER_COUNT: usize = 5;
const MAX_CONFIG_SIZE_BYTES: usize = 4096;
// max body size caps the size of our io file, though we have the config header additionally and it
// will get truncated up to nearest 2 MB alignment, so really it should be about MAX_BODY_SIZE +
// 2MB since the config should always fit in 2 MB anyways
const MAX_BODY_SIZE: usize = 0xa00000; // 10 MB

struct Config {
    addr: SocketAddr,
    total_conn: usize,
    timeout_per_conn: Duration,
    max_requests_per_conn: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiV1Data {
    pub image: String,
}

// #[derive(Debug)]
// enum Error {
//     BadRequest(&'static str),
// }

async fn send_response(mut socket: TcpStream, status: usize, message: &str) -> io::Result<()> {
    let l = message.as_bytes().len();
    let s = format!("HTTP/1.1 {status}\r\nContent-length: {l}\r\n\r\n{message}");
    let data = s.as_str().as_bytes();
    socket.write_all(&data).await
}

//async fn process_api_v1_i(mut socket: TcpStream, content_length: usize, mut body_start: Vec<u8>) {
//    if content_length < 4 { return send_response(socket, 400, "body too short").await; }
//
//    // TODO why the heck is it so hard to read into a vec's spare capacity???
//    let mut buf = Box::new([0; MAX_CONFIG_SIZE_BYTES]);
//    if body_start.len() < 4 {
//        match socket.read(buf.as_mut()).await {
//            // TODO Ok(0) ?
//            Ok(n) => {
//                body_start.extend_from_slice(&buf[..n]);
//            }
//            Err(_) => { return send_response(socket, 400, "error bad read").await; }
//        }
//    }
//    // yeah its possible we don't get the full bit at this point but those clients can deal
//    if body_start.len() < 4 { return send_response(socket, 400, "error too short").await; }
//
//    let config_size = {
//        let mut cursor = Cursor::new(&body_start);
//        match ReadBytesExt::read_u32::<LE>(&mut cursor) {
//            Ok(config_size) => {
//                let config_size = config_size as usize;
//                if config_size + 4 > content_length {
//                    return send_response(socket, 400, "bad config size").await;
//                }
//                if config_size + 4 > body_start.len() {
//                    return send_response(socket, 400, "should have gotten enough body already").await;
//                }
//                config_size
//            }
//            _ => { return send_response(socket, 400, "got bad config size").await; }
//        }
//    };
//    let archive_size = content_length - config_size - 4;  // checked above
//
//    let api_v1_data: ApiV1Data = {
//        let config_buf = &body_start[4..4+config_size];
//        match serde_json::from_slice(config_buf) {
//            Ok(d) => d,
//            Err(_) => { return send_response(socket, 400, "bad api data").await; }
//        }
//    };
//    eprintln!("got api data config_size={config_size} archive_size={archive_size} {api_v1_data:?}");
//
//    return send_response(socket, 200, "wip").await;
//}
//
//async fn process_request(socket: TcpStream, method: &str, route: &str, content_length: Option<usize>, body_start: Vec<u8>) {
//    match (method, route, content_length) {
//        ("GET" , "/", _) => { return send_response(socket, 200, format!("hello world {content_length:?}").as_str()).await; }
//        ("POST", "/api/v1/i", Some(l)) => { return process_api_v1_i(socket, l, body_start).await; }
//        ("POST", "/api/v1/i", None)    => { return send_response(socket, 400, "bad content length").await; }
//        _  => { return send_response(socket, 404, "").await; }
//    }
//}
//
//// read headers until we have a full connection and maybe start of a body
//async fn process1(mut socket: TcpStream, addr: SocketAddr) -> io::Result<Error> {
//    eprintln!("connection from {addr}");
//    let mut offset = 0;
//    loop {
//        // WOW this bug is here
//        let mut buf = Box::new([0; MAX_HEADER_SIZE_BYTES]);
//        let n_read = match socket.read(&mut buf.as_mut()[offset..]).await {
//            Ok(n) => n,
//            Err(e) => { eprintln!("err {e}"); return send_response(socket, 400, "oh no err").await; },
//        };
//        let valid = &buf.as_ref()[..n_read + offset];
//        offset = valid.len();
//        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADER_COUNT]; // Box?
//        let mut request = httparse::Request::new(&mut headers[..]);
//        match request.parse(valid) {
//            Ok(httparse::Status::Complete(body_start)) => {
//                let body = &valid[body_start..];
//                if request.version != Some(1) { return send_response(socket, 400, "version not 1.1").await; }
//                let content_length = match content_length(&request) {
//                    None => None,
//                    Some(l) if l as usize > MAX_BODY_SIZE => None,
//                    Some(l) => Some(l as usize),
//                };
//                match (request.method, request.path) {
//                    (None, _) | (_, None) => { return send_response(socket, 400, "missing method or path").await; }
//                    (Some(method), Some(route)) => {
//                        // do these behave like tail calls? I don't think so with the await
//                        return process_request(socket, method, route, content_length, body.to_vec()).await;
//                    }
//                }
//            }
//            Ok(httparse::Status::Partial) => {
//                if offset >= MAX_HEADER_SIZE_BYTES {
//                    return send_response(socket, 400, "too long").await;
//                }
//            }
//            Err(e) => { eprintln!("err {e}"); return send_response(socket, 400, "oh no err").await; },
//        }
//    }
//    //return send_response(socket, 200, "good job").await;
//}
//async fn process1_3(mut socket: TcpStream, addr: SocketAddr) -> io::Result<Error> {
//    // read at least 4 to get sizeof config
//    // read at least config size
//    // parse
//}

//async fn parse_request<'a>(buf: &'a mut Box<[u8; MAX_HEADER_SIZE_BYTES]>, socket: &mut TcpStream, request: &mut httparse::Request<'a, 'a>) -> io::Result<usize> {
//    let mut offset = 0;
//    loop {
//        let n_read = socket.read(&mut buf.as_mut()[offset..]).await?;
//        let valid = &buf[..n_read + offset];
//        offset = valid.len();
//        match request.parse(valid) {
//            Ok(httparse::Status::Complete(body_start)) => {
//                return Ok(body_start)
//            }
//            Ok(httparse::Status::Partial) => {
//                if offset >= MAX_HEADER_SIZE_BYTES {
//                    return Err(ErrorKind::InvalidData.into());
//                }
//            }
//            Err(e) => {
//                eprintln!("error parsing request {e}");
//                return Err(ErrorKind::InvalidData.into());
//            }
//        }
//    }
//}

async fn handle_conn(mut socket: TcpStream, addr: SocketAddr, max_requests: usize) -> io::Result<()> {
    eprintln!("handling conn from {addr}");
    let mut buf = Box::new([0; MAX_HEADER_SIZE_BYTES]);
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADER_COUNT]; // Box?
    let mut request = httparse::Request::new(&mut headers[..]);
    for _ in 0..max_requests {
        let body_start = {
            let mut offset = 0;
            loop {
                let n_read = socket.read(&mut buf.as_mut()[offset..]).await?;
                let valid = &buf[..n_read + offset];
                offset = valid.len();
                match request.parse(valid) {
                    Ok(httparse::Status::Complete(body_start)) => {
                        break body_start;
                    }
                    Ok(httparse::Status::Partial) => {
                        if offset >= MAX_HEADER_SIZE_BYTES {
                            return Err(ErrorKind::InvalidData.into());
                        }
                    }
                    Err(e) => {
                        eprintln!("error parsing request {e}");
                        return Err(ErrorKind::InvalidData.into());
                    }
                }
            }
        };
        eprintln!("body_start {body_start}");
    }
    Ok(())
}

async fn serve(config: Config) {
    // Bind the listener to the address
    let listener = TcpListener::bind(config.addr).await.unwrap();

    loop {
        // The second item contains the IP and port of the new connection.
        let (socket, addr) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            match handle_conn(socket, addr, config.max_requests_per_conn).await {
                Ok(()) => {},
                Err(e) => {
                    eprintln!("got err {e}");
                }
            }
        });
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8000")]
    addr: String,
}


#[tokio::main(flavor="current_thread")]
async fn main() {
    let args = Args::parse();
    let addr = args.addr.parse().expect("bad socket addr");
    let config = Config {
        addr: addr,
        total_conn: 4,
        timeout_per_conn: Duration::from_millis(1000 * 5),
        max_requests_per_conn: 4,
    };
    () = serve(config).await;
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
