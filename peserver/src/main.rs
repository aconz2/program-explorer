use std::time::Duration;
use std::io::Write;
use std::io;
use std::ffi::OsString;

use async_trait::async_trait;
use bytes::{Bytes,BytesMut};
use http;
use http::{Method, Response, StatusCode};
use pingora_timeout::timeout;

use tempfile::NamedTempFile;

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

use peinit;
use perunner::{worker,UID,NIDS,create_runtime_spec};
use perunner::cloudhypervisor::{CloudHypervisorConfig};
use peimage::PEImageMultiIndex;

struct HttpRunnerApp {
    pub pool: worker::asynk::Pool,
    pub index: PEImageMultiIndex,
    pub cloud_hypervisor: OsString,
    pub initramfs: OsString,
    pub kernel: OsString,
}

impl HttpRunnerApp {
    //fn new(pool: worker::Pool, index: PEImageMultiIndex) -> Self {
    //    Self { pool: pool, index: index}
    //}
}

mod apiv1 {
    pub mod runi {
    use serde::{Deserialize,Serialize};
        #[derive(Deserialize)]
        pub struct Request {
            pub image: String,
            pub stdin: Option<String>,
            pub args: Vec<String>,
        }

        #[derive(Serialize)]
        pub struct Response {
        }
    }
}

async fn read_full_body(http_stream: &mut ServerSession) -> Result<Bytes, Box<pingora::Error>> {
    let mut acc = BytesMut::with_capacity(4096);
    loop {
        match http_stream.read_request_body().await? {
            Some(bytes) => {
                acc.extend_from_slice(&bytes);
            }
            None => {
                break;
            }
        }
    }
    Ok(acc.freeze())
}

fn parse_api_request(buf: &[u8]) -> Option<apiv1::runi::Request> {
    if buf.len() < 4 { return None; }
    let json_size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    serde_json::from_slice(&buf[4..4+json_size]).ok()
}

fn response_no_body(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_LENGTH, 0)
        .body(vec![])
        .unwrap()
}

fn response_with_message(status: StatusCode, message: &str) -> Response<Vec<u8>> {
    let body: Vec<_> = message.into();
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        .unwrap()
}

enum Error {
    ReadTimeout,
    ReadError,
    BadRequest,
    NoSuchImage,
    TempfileCreate,
    TempfileWrite,
    QueueFull,
    WorkerRecv,
}

impl Into<StatusCode> for Error {
    fn into(self: Error) -> StatusCode {
        use Error::*;
        match self {
            ReadTimeout => StatusCode::REQUEST_TIMEOUT,
            ReadError |
            NoSuchImage |
            BadRequest => StatusCode::BAD_REQUEST,
            QueueFull => StatusCode::SERVICE_UNAVAILABLE,
            WorkerRecv |
            TempfileCreate |
            TempfileWrite => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl Into<Response<Vec<u8>>> for Error {
    fn into(self: Error) -> Response<Vec<u8>> {
        response_no_body(self.into())
    }
}

impl HttpRunnerApp {
    async fn apiv1_runi(&self, http_stream: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        let read_timeout = Duration::from_millis(2000);
        // TODO ideally could read this in two parts to send the rest to the file
        // two unwraps, one for timeout and the other for read errors
        let body = timeout(read_timeout, read_full_body(http_stream))
            .await
            .map_err(|_| Error::ReadTimeout)?
            .map_err(|_| Error::ReadError)?;

        let api_req = parse_api_request(&body).ok_or(Error::BadRequest)?;

        let image_entry = self.index.get(&api_req.image).ok_or(Error::NoSuchImage)?;

        let runtime_spec = create_runtime_spec(&image_entry.image.config, &api_req.args)
            .ok_or(Error::BadRequest)?;

        let timeout = Duration::from_millis(1000);
        let ch_timeout = timeout + Duration::from_millis(500);

        let ch_config = CloudHypervisorConfig {
            bin           : self.cloud_hypervisor.clone(),
            kernel        : self.kernel.clone(),
            initramfs     : self.initramfs.clone(),
            log_level     : None,
            console       : false,
            keep_args     : true,
            event_monitor : false,
        };

        let pe_config = peinit::Config {
            timeout            : timeout,
            oci_runtime_config : serde_json::to_string(&runtime_spec).unwrap(),
            uid_gid            : UID,
            nids               : NIDS,
            stdin              : api_req.stdin,
            strace             : false,
            crun_debug         : false,
            rootfs_dir         : image_entry.image.rootfs.clone(),
            rootfs_kind        : image_entry.rootfs_kind,
        };

        let mut io_file = NamedTempFile::new().map_err(|_| Error::TempfileCreate)?;
        () = io_file.write_all(&body).map_err(|_| Error::TempfileWrite)?;

        let worker_input = worker::Input {
            id         : 42,
            pe_config  : pe_config,
            ch_config  : ch_config,
            ch_timeout : ch_timeout,
            io_file    : io_file,
            rootfs     : image_entry.path.clone().into(),
        };

        let (resp_sender, resp_receiver) = tokio::sync::oneshot::channel();

        () = self.pool.sender()
            .try_send((worker_input, resp_sender))
            .map_err(|_| {
                eprintln!("todo, queue was full, we probably shouldn't have gotten this work item, or maybe somehow interface better with the sync thread pool so we can wait");
                Error::QueueFull
            })?;

        let worker_output = resp_receiver.await.map_err(|_| Error::WorkerRecv)?;

        Ok(response_with_message(StatusCode::OK, "todo"))
    }
}

#[async_trait]
impl ServeHttp for HttpRunnerApp {
    async fn response(&self, http_stream: &mut ServerSession) -> Response<Vec<u8>> {
        let req_parts: &http::request::Parts = http_stream.req_header();
        match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::POST, "/api/v1/runi") => self.apiv1_runi(http_stream).await.unwrap_or_else(|e| e.into()),
            _ => {
                response_no_body(StatusCode::NOT_FOUND)
            }
        }


        //let mut tf = NamedTempFile::new().unwrap();
        //tf.write_all(&body).unwrap();

    }
}

fn main() {
    //let opt = Some(Opt::parse_args());
    let opt = Some(Opt {
        upgrade: false,
        daemon: false,
        nocapture: false,
        test: false,
        conf: None // path to configuration file
    });

    let cwd = std::env::current_dir().unwrap();
    let mut my_server = Server::new(opt).unwrap();
    my_server.bootstrap();

    let pool = worker::asynk::Pool::new(&worker::cpuset(2, 2, 2).unwrap());
    let index = PEImageMultiIndex::from_paths(&["../ocismall.erofs".into()]).unwrap();
    let app = HttpRunnerApp {
        pool             : pool,
        index            : index,
        kernel           : cwd.join("../vmlinux").into(),
        initramfs        : cwd.join("../initramfs").into(),
        cloud_hypervisor : cwd.join("../cloud-hypervisor").into(),
    };
    let mut runner_service_http = Service::new("Echo Service HTTP".to_string(), app);
    runner_service_http.add_tcp("127.0.0.1:8080");

    let services: Vec<Box<dyn IService>> = vec![
        Box::new(runner_service_http),
    ];
    my_server.add_services(services);
    my_server.run_forever();
}
