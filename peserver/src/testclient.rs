use std::io::{Read, Write};
use std::path::Path;

use bytes::Bytes;
use clap::Parser;
use flate2::read::GzDecoder;
use http::Method;
use pingora::prelude::{HttpPeer, RequestHeader};
use std::time::Duration;

use pearchive::{unpack_visitor, PackMemToVec, PackMemVisitor, UnpackVisitor};
use peserver::api;
use peserver::api::v2 as apiv2;

use peserver::util::read_full_client_response_body;

fn escape_dump(input: &[u8]) {
    let mut output = Vec::<u8>::with_capacity(1024);
    for b in input.iter() {
        for e in std::ascii::escape_default(*b) {
            output.push(e);
        }
    }
    std::io::stdout().write_all(output.as_slice()).unwrap();
    println!();
}

fn zcat(input: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut gz = GzDecoder::new(input);
    let mut ret = Vec::with_capacity(4096);
    gz.read_to_end(&mut ret)?;
    Ok(ret)
}

struct UnpackVisitorPrinter {}

impl UnpackVisitor for UnpackVisitorPrinter {
    fn on_file(&mut self, name: &Path, data: &[u8]) -> bool {
        println!("=== {:?} ({}) ===", name, data.len());
        if !data.is_empty() {
            escape_dump(data);
        }
        true
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "localhost:6188")]
    addr: String,

    #[arg(long, default_value = "index.docker.io/library/busybox:1.36.0")]
    image: String,

    #[arg(long)]
    stdin: Option<String>,

    #[arg(long)]
    env: Vec<String>,

    #[arg(long)]
    gzip: bool,

    #[arg(long)]
    body_too_big: bool,

    #[arg(long)]
    header_too_many: bool,

    #[arg(long)]
    header_too_big: bool,

    #[arg(long)]
    corrupt_body: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn print_headers(prefix: &str, headers: &http::HeaderMap) {
    for (k, v) in headers.iter() {
        println!("{}{}: {:?}", prefix, k, v);
    }
}

//fn hexdump(buf: &[u8]) {
//    for chunk in buf.chunks(16) {
//        for byte in chunk {
//            print!("{:02x} ", byte);
//        }
//        println!();
//    }
//}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let connector = pingora::connectors::http::v1::Connector::new(None);
    let peer = if args.addr.starts_with("/") {
        HttpPeer::new_uds(&args.addr, false, "".to_string()).unwrap()
    } else {
        HttpPeer::new(&args.addr, false, "".to_string())
    };
    let (mut session, _) = connector.get_http_session(&peer).await.unwrap();
    session.read_timeout = Some(Duration::from_secs(5));
    session.write_timeout = Some(Duration::from_secs(5));

    let api_req = apiv2::runi::Request {
        cmd: Some(args.args),
        entrypoint: Some(vec![]),
        stdin: args.stdin,
        env: Some(args.env),
    };

    let buf = {
        let json = serde_json::to_vec(&api_req).unwrap();
        let jsonlen: u32 = json.len().try_into().unwrap();
        let mut buf: Vec<u8> = jsonlen.to_le_bytes().into_iter().collect();
        buf.extend_from_slice(&json);
        let mut v = PackMemToVec::with_vec(buf);
        v.file("file1", b"data1").unwrap();
        if args.body_too_big {
            let too_much_data = vec![0; 65536];
            v.file("file2", &too_much_data).unwrap();
        }
        if args.corrupt_body {
            let mut v = v.into_vec().unwrap();
            v.push(4); // BadTag
            v
        } else {
            v.into_vec().unwrap()
        }
    };

    let url = apiv2::runi::PREFIX.to_owned() + &args.image;
    let req = {
        let mut x = RequestHeader::build(Method::POST, url.as_bytes(), Some(3)).unwrap();
        x.insert_header("Content-Type", api::APPLICATION_X_PE_ARCHIVEV1)
            .unwrap();
        x.insert_header("Content-Length", buf.len()).unwrap();
        if args.gzip {
            x.insert_header("Accept-Encoding", "gzip").unwrap();
        }
        if args.header_too_many {
            for i in 0..1000 {
                x.insert_header(format!("my-header-{}", i), "blah-blah-blah")
                    .unwrap();
            }
        }
        if args.header_too_big {
            // okay doesn't seem like there is an upper limit yet...
            let mut s = String::with_capacity(4096 * 16);
            for _ in 0..s.capacity() {
                s.push('x');
            }
            x.insert_header("my-big-header", s).unwrap();
        }
        Box::new(x)
    };

    println!("{} {:?} {}", req.method, req.version, req.uri);
    print_headers("> ", &req.headers);

    let _ = session.write_request_header(req).await.unwrap();
    let _ = session.write_body(&buf).await.unwrap();
    let _ = session.read_response().await.unwrap();
    let res_parts: &http::response::Parts = session.resp_header().unwrap();
    let status = res_parts.status;

    println!("{} {:?}", status, res_parts.version);
    print_headers("< ", &res_parts.headers);

    if args.gzip
        && res_parts
            .headers
            .get("Content-encoding")
            .and_then(|x| x.to_str().ok())
            != Some("gzip")
    {
        println!("yoooooooooooooooooo gzip not there");
    }

    let body = {
        let body = read_full_client_response_body(&mut session).await.unwrap();
        if args.gzip {
            Bytes::from(zcat(&body).unwrap())
        } else {
            body
        }
    };
    if status != 200 {
        println!("ERROR {:?}", body);
        return;
    }
    //hexdump(&body[..min(body.len(), 256)]);
    let (response, archive) = apiv2::runi::parse_response(&body).unwrap();
    println!("api  response {:#?}", response);

    let mut unpacker = UnpackVisitorPrinter {};
    unpack_visitor(archive, &mut unpacker).unwrap();
}
