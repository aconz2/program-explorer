//use std::net::{TcpStream,SocketAddr};
use http::Method;
use std::time::Duration;
use pingora::prelude::{RequestHeader,HttpPeer};
use clap::Parser;

use peserver::api;
use peserver::api::v1 as apiv1;
use pearchive::{PackMemToVec,PackMemVisitor};

mod util;
use util::read_full_body;

fn escape_dump(input: &[u8]) {
    use std::io::Write;
    let mut output = Vec::<u8>::with_capacity(1024);
    for b in input.iter() {
        for e in std::ascii::escape_default(*b) {
            output.push(e);
        }
    }
    let _ = std::io::stdout().write_all(output.as_slice()).unwrap();
    println!("");
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:6188")]
    addr: String,

    #[arg(long, default_value = "sha256:22f27168517de1f58dae0ad51eacf1527e7e7ccc47512d3946f56bdbe913f564")]
    image: String,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[tokio::main]
async fn main() {

    let args = Args::parse();

    let connector = pingora::connectors::http::v1::Connector::new(None);
    let peer = HttpPeer::new(args.addr, false, "".to_string());
    let (mut session, _) = connector.get_http_session(&peer).await.unwrap();
    session.read_timeout = Some(Duration::from_secs(5));
    session.write_timeout = Some(Duration::from_secs(5));

    let api_req = apiv1::runi::Request {
        cmd: Some(args.args),
        entrypoint: Some(vec![]),
        stdin: None,
    };

    let buf = {
        let json = serde_json::to_vec(&api_req).unwrap();
        let jsonlen: u32 = json.len().try_into().unwrap();
        let mut buf: Vec<u8> = jsonlen.to_le_bytes().into_iter().collect();
        buf.extend_from_slice(&json);
        let mut v = PackMemToVec::with_vec(buf);
        v.file("file1", b"data1").unwrap();
        v.into_vec().unwrap()
    };

    let url = apiv1::runi::PREFIX.to_owned() + &args.image;
    let req = {
        let mut x = RequestHeader::build(Method::POST, url.as_bytes(), Some(2)).unwrap();
        x.insert_header("Content-Type", api::APPLICATION_X_PE_ARCHIVEV1).unwrap();
        x.insert_header("Content-Length", buf.len()).unwrap();
        Box::new(x)
    };
    println!("request {:?}", req);
    let _ = session.write_request_header(req).await.unwrap();
    let _ = session.write_body(&buf).await.unwrap();
    let _ = session.read_response().await.unwrap();
    let res_parts: &http::response::Parts = session.resp_header().unwrap();
    println!("response {:?}", res_parts);

    let body = read_full_body(&mut session).await.unwrap();

    //
    escape_dump(&body);
}
