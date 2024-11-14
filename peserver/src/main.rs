use async_trait::async_trait;
use bytes::Bytes;
use http::{Response, StatusCode};
//use log::debug;
use pingora_timeout::timeout;
//use std::sync::Arc;
use std::time::Duration;
//use tokio::io::{AsyncReadExt, AsyncWriteExt};

use pingora::services::Service as IService;
use pingora::services::listening::Service;
use pingora::server::Server;
use pingora::server::configuration::Opt;
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::ServerSession;
//use pingora::protocols::Stream;
//use pingora::server::ShutdownWatch;
//use clap::Parser;
//use clap::derive::Parser;

struct HttpEchoApp();

#[async_trait]
impl ServeHttp for HttpEchoApp {
    async fn response(&self, http_stream: &mut ServerSession) -> Response<Vec<u8>> {
        // read timeout of 2s
        let read_timeout = 2000;
        let body = match timeout(
            Duration::from_millis(read_timeout),
            http_stream.read_request_body(),
        )
        .await
        {
            Ok(res) => match res.unwrap() {
                Some(bytes) => bytes,
                None => Bytes::from("no body!"),
            },
            Err(_) => {
                panic!("Timed out after {:?}ms", read_timeout);
            }
        };

        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/html")
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(body.to_vec())
            .unwrap()
    }
}

fn main() {
    let opt = Some(Opt::parse_args());
    let mut my_server = Server::new(opt).unwrap();
    my_server.bootstrap();

    let mut echo_service_http = Service::new("Echo Service HTTP".to_string(), HttpEchoApp());
    echo_service_http.add_tcp("127.0.0.1:8080");

    let services: Vec<Box<dyn IService>> = vec![
        Box::new(echo_service_http),
    ];
    my_server.add_services(services);
    my_server.run_forever();
}
