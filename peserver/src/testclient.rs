//use std::net::{TcpStream,SocketAddr};
//use clap::Parser;
//use std::io::{Read,Write};
//
//mod main;
//use main::ApiV1Data;
//
//fn escape_dump(input: &[u8]) {
//    let mut output = Vec::<u8>::with_capacity(1024);
//    for b in input.iter() {
//        for e in std::ascii::escape_default(*b) {
//            output.push(e);
//        }
//    }
//    let _ = std::io::stdout().write_all(output.as_slice()).unwrap();
//    println!("");
//}
//
//#[derive(Parser, Debug)]
//#[command(version, about, long_about = None)]
//struct Args {
//    #[arg(long, default_value = "127.0.0.1:8000")]
//    addr: String,
//}
//
fn main() {
//    let args = Args::parse();
//    let addr: SocketAddr = args.addr.parse().expect("bad socket addr");
//
//    let mut conn = TcpStream::connect(addr).expect("couldn't connect");
//    let req_header = ApiV1Data {
//        image: "index.docker.io/busybox:1.32".to_string(),
//    };
//    let req_header_bytes = serde_json::to_vec(&req_header).expect("json ser");
//    let req_header_len: u32 = req_header_bytes.len().try_into().expect("should fit");
//    let l = req_header_bytes.len() + 4;
//    let data = format!("POST /api/v1/i HTTP/1.1\r\nContent-length: {l}\r\n\r\n");
//
//    eprintln!("writing content-length: {l} data size {}", req_header_bytes.len());
//
//    let mut full_data = Vec::new();
//    full_data.extend_from_slice(data.as_str().as_bytes());
//    full_data.extend_from_slice(&req_header_len.to_le_bytes());
//    full_data.extend_from_slice(&req_header_bytes);
//
//    conn.write_all(full_data.as_slice()).expect("bad write");
//
//    let mut buf = [0; 4096];
//    let n = conn.read(&mut buf).expect("bad read");
//    escape_dump(&buf[..n]);
}
