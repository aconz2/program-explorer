use std::time::Duration;
use std::io::{Read,Write};
use std::ffi::OsString;

use async_trait::async_trait;
use bytes::{Bytes,BytesMut};
use http;
use http::{Method, Response, StatusCode};
use tempfile::NamedTempFile;

use pingora_timeout::timeout;
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
use perunner::cloudhypervisor::{CloudHypervisorConfig,round_up_file_to_pmem_size};
use peimage::PEImageMultiIndex;

const APPLICATION_JSON: &str = "application/json";
const APPLICATION_X_PE_ARCHIVEV1: &str = "application/x.pe.archivev1";

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

enum ContentType {
    ApplicationJson,
    PeArchiveV1, // <u32 json size> <json> <pearchivev1>
}

impl TryFrom<&str> for ContentType {
    type Error = ();

    fn try_from(s: &str) -> Result<ContentType, ()> {
        match s {
            APPLICATION_JSON => Ok(ContentType::ApplicationJson),
            APPLICATION_X_PE_ARCHIVEV1 => Ok(ContentType::PeArchiveV1),
            _ => Err(()),
        }
    }

}

impl Into<&str> for ContentType {
    fn into(self) -> &'static str {
        match self {
            ContentType::ApplicationJson => APPLICATION_JSON,
            ContentType::PeArchiveV1 => APPLICATION_X_PE_ARCHIVEV1,
        }
    }
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

fn parse_apiv1_runi_request(buf: &[u8], content_type: &ContentType) -> Option<apiv1::runi::Request> {
    match content_type {
        ContentType::ApplicationJson => {
            serde_json::from_slice(&buf).ok()
        }
        ContentType::PeArchiveV1 => {
            if buf.len() < 4 { return None; }
            let json_size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            serde_json::from_slice(&buf[4..4+json_size]).ok()
        }
    }
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

#[derive(Debug)]
enum Error {
    ReadTimeout,
    ReadError,
    BadRequest,
    NoSuchImage,
    TempfileCreate,
    QueueFull,
    WorkerRecv,
    BadContentType,
    ResponseRead,
    WorkerError,
    ResponseBuild,
    Internal,
}

impl Into<StatusCode> for Error {
    fn into(self: Error) -> StatusCode {
        use Error::*;
        eprintln!("got error {:?}", self);
        match self {
            ReadTimeout => StatusCode::REQUEST_TIMEOUT,
            ReadError |
            NoSuchImage |
            BadContentType |
            BadRequest => StatusCode::BAD_REQUEST,
            QueueFull => StatusCode::SERVICE_UNAVAILABLE,
            WorkerRecv |
            TempfileCreate |
            ResponseRead |
            WorkerError |
            ResponseBuild |
            Internal => StatusCode::INTERNAL_SERVER_ERROR,
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

        let content_type = http_stream.req_header()
            .headers
            .get("Content-Type")
            .and_then(|x| x.to_str().ok())
            .and_then(|x| x.try_into().ok())
            .ok_or(Error::BadContentType)?;

        // not using accept header
        let response_format = match content_type {
            ContentType::ApplicationJson => peinit::ResponseFormat::JsonV1,
            ContentType::PeArchiveV1 => peinit::ResponseFormat::PeArchiveV1,
        };

        let api_req = parse_apiv1_runi_request(&body, &content_type).ok_or(Error::BadRequest)?;

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
            console       : true,
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
            response_format    : response_format,
        };

        let mut io_file = NamedTempFile::new().map_err(|_| Error::TempfileCreate)?;
        match content_type {
            ContentType::ApplicationJson => {
                // this is blocking, but is going to tmpfs so I don't think its bad to do this?
                peinit::write_io_file_config(io_file.as_file_mut(), &pe_config).map_err(|_| Error::Internal)?;
                let _ = round_up_file_to_pmem_size(io_file.as_file_mut()).map_err(|_| Error::Internal)?;
            }
            ContentType::PeArchiveV1 => {
                todo!()
            }
        }

        let worker_input = worker::Input {
            id         : 42, // id is useless because we are passing a return channel
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

        let mut worker_output = resp_receiver
            .await
            .map_err(|_| Error::WorkerRecv)?
            .map_err(|postmortem| {
                fn dump_file<F: Read>(name: &str, file: &mut F) {
                    eprintln!("=== {} ===", name);
                    let _ = std::io::copy(file, &mut std::io::stderr());
                }
                eprintln!("worker error {:?}", postmortem.error);
                if let Some(args) = postmortem.args { eprintln!("launched ch with {:?}", args); };
                if let Some(mut err_file) = postmortem.logs.err_file { dump_file("ch err", &mut err_file); }
                if let Some(mut log_file) = postmortem.logs.log_file { dump_file("ch log", &mut log_file); }
                if let Some(mut con_file) = postmortem.logs.con_file { dump_file("ch con", &mut con_file); }
                Error::WorkerError
            })?;

        if true {
            fn dump_file<F: Read>(name: &str, file: &mut F) {
                eprintln!("=== {} ===", name);
                let _ = std::io::copy(file, &mut std::io::stderr());
            }
            if let Some(mut err_file) = worker_output.ch_logs.err_file { dump_file("ch err", &mut err_file); }
            if let Some(mut log_file) = worker_output.ch_logs.log_file { dump_file("ch log", &mut log_file); }
            if let Some(mut con_file) = worker_output.ch_logs.con_file { dump_file("ch con", &mut con_file); }
        }

        match response_format {
            peinit::ResponseFormat::JsonV1 => {
                let response_json_serialized = {
                    use std::io::{Seek,SeekFrom};
                    worker_output.io_file.seek(SeekFrom::Start(0)).map_err(|_| Error::ResponseRead)?;
                    let (_archive_size, response_bytes) = peinit::read_io_file_response_bytes(worker_output.io_file.as_file_mut())
                        .map_err(|_| Error::ResponseRead)?;
                    response_bytes
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header(http::header::CONTENT_LENGTH, response_json_serialized.len())
                    .header(http::header::CONTENT_TYPE, APPLICATION_JSON)
                    .body(response_json_serialized)
                    .map_err(|_| Error::ResponseBuild)
            }
            _ => todo!()
        }
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
        cloud_hypervisor : cwd.join("../cloud-hypervisor-static").into(),
    };
    let mut runner_service_http = Service::new("Echo Service HTTP".to_string(), app);
    runner_service_http.add_tcp("127.0.0.1:8080");

    let services: Vec<Box<dyn IService>> = vec![
        Box::new(runner_service_http),
    ];
    my_server.add_services(services);
    my_server.run_forever();
}
