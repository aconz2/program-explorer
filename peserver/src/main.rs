use std::time::Duration;
use std::io::{Read};
use std::ffi::OsString;

use async_trait::async_trait;
use bytes::{Bytes,BytesMut};
use http;
use http::{Method, Response, StatusCode};
use tempfile::NamedTempFile;
use serde_json;
use serde::{Serialize};
use env_logger;
use log::{trace};

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
const ROUTE_API_V1_RUNI: &str = "/api/v1/runi/";

#[derive(Debug)]
enum Error {
    ReadTimeout,
    ReadError,
    BadRequest,
    BadImagePath,
    NoSuchImage,
    TempfileCreate,
    QueueFull,
    WorkerRecv,
    BadContentType,
    ResponseRead,
    WorkerError,
    Internal,
    Serialize,
}

struct HttpRunnerApp {
    pub pool: worker::asynk::Pool,
    pub index: PEImageMultiIndex,
    pub cloud_hypervisor: OsString,
    pub initramfs: OsString,
    pub kernel: OsString,
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
        use peinit;

        #[serde(deny_unknown_fields)]
        #[derive(Deserialize)]
        pub struct Request {
            pub stdin      : Option<String>,       // filename that will be set as stdin, noop
                                                   // for content-type: application/json
            pub entrypoint : Option<Vec<String>>,  // as per oci image config
            pub cmd        : Option<Vec<String>>,  // as per oci image config
        }

        type Response = peinit::Response;
    }

    pub mod images {
        use serde::Serialize;
        use peimage;
        use oci_spec::image as oci_image;

        #[derive(Serialize)]
        pub struct Image {
            pub uri: String,
            pub info: peimage::PEImageId,
            pub config: oci_image::ImageConfiguration,
            pub manifest: oci_image::ImageManifest,
        }

        #[derive(Serialize)]
        pub struct Response {
            pub images: Vec<Image>,
        }

        impl From<&peimage::PEImageMultiIndex> for Response {
            fn from(index: &peimage::PEImageMultiIndex) -> Self {
                let images: Vec<_> = index.map().iter()
                    .map(|(_k, v)| {
                        Image {
                            uri: v.image.id.digest.replace(":", "/"),
                            info: v.image.id.clone(),
                            config: v.image.config.clone(),
                            manifest: v.image.manifest.clone(),
                        }
                    })
                    .collect();
                Self { images }
            }
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

fn parse_apiv1_runi_request(body: &[u8], content_type: &ContentType) -> Option<apiv1::runi::Request> {
    match content_type {
        ContentType::ApplicationJson => {
            serde_json::from_slice(&body).ok()
        }
        ContentType::PeArchiveV1 => {
            if body.len() < 4 { return None; }
            let json_size = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
            serde_json::from_slice(&body[4..4+json_size]).ok()
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

fn response_json<T: Serialize>(status: StatusCode, body: T) -> serde_json::Result<Response<Vec<u8>>> {
    Ok(response_json_vec(status, serde_json::to_vec(&body)?))
}

fn response_json_vec(status: StatusCode, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, APPLICATION_JSON)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
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

impl Into<StatusCode> for Error {
    fn into(self: Error) -> StatusCode {
        use Error::*;
        eprintln!("got error {:?}", self);
        match self {
            ReadTimeout => StatusCode::REQUEST_TIMEOUT,
            ReadError |
            NoSuchImage |
            BadContentType |
            BadImagePath |
            BadRequest => StatusCode::BAD_REQUEST,
            QueueFull => StatusCode::SERVICE_UNAVAILABLE,
            WorkerRecv |
            TempfileCreate |
            ResponseRead |
            WorkerError |
            Serialize |
            Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl Into<Response<Vec<u8>>> for Error {
    fn into(self: Error) -> Response<Vec<u8>> {
        response_no_body(self.into())
    }
}

// /api/v1/runi/<algo>/<digest>
//              [-------------]
// doesn't fully check things, but covers the basics
fn parse_apiv1_runi_path(s: &str) -> Option<&str> {
    //if !s.starts_with(ROUTE_API_V1_RUNI) { return None; }
    let x = s.strip_prefix(ROUTE_API_V1_RUNI)?;
    if x.len() > 135 { return None; }  // this is length of sha512:...
    let slash_i = x.find("/")?;
    if x[slash_i+1..].contains("/") { return None; }
    Some(x)
}

impl HttpRunnerApp {
    async fn apiv1_runi(&self, http_stream: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        let req_parts: &http::request::Parts = http_stream.req_header();
        // this is like sha256/abcdefg1234 with the slash, not colon
        let uri_path_image = parse_apiv1_runi_path(req_parts.uri.path())
            .ok_or(Error::BadImagePath)?;

        let image_entry = self.index.get(&uri_path_image)
            .ok_or(Error::NoSuchImage)?;

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

        // TODO why aren't we using the bulitin read timeout on the http_stream?
        let read_timeout = Duration::from_millis(2000);
        // TODO ideally could read this in two parts to send the rest to the file
        let body = timeout(read_timeout, read_full_body(http_stream))
            .await
            .map_err(|_| Error::ReadTimeout)?
            .map_err(|_| Error::ReadError)?;

        let api_req = parse_apiv1_runi_request(&body, &content_type).ok_or(Error::BadRequest)?;

        let runtime_spec = create_runtime_spec(&image_entry.image.config, api_req.entrypoint.as_deref(), api_req.cmd.as_deref())
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
            stdin              : api_req.stdin,
            strace             : false,
            crun_debug         : false,
            rootfs_dir         : image_entry.image.rootfs.clone(),
            rootfs_kind        : image_entry.rootfs_kind,
            response_format    : response_format,
            kernel_inspect     : false,
        };

        let mut io_file = NamedTempFile::new().map_err(|_| Error::TempfileCreate)?;
        match content_type {
            ContentType::ApplicationJson => {
                // this is blocking, but is going to tmpfs so I don't think its bad to do this?
                peinit::write_io_file_config(io_file.as_file_mut(), &pe_config).map_err(|_| Error::Internal)?;
                let _ = round_up_file_to_pmem_size(io_file.as_file_mut()).map_err(|_| Error::Internal)?;
            }
            ContentType::PeArchiveV1 => {
                todo!("need offset of archive buf from when we parse");
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
                    peinit::read_io_file_response_bytes(worker_output.io_file.as_file_mut())
                        .map_err(|_| Error::ResponseRead)
                        .map(|(_, x)| x)?
                };
                Ok(response_json_vec(StatusCode::OK, response_json_serialized))
            }
            _ => todo!()
        }
    }

    async fn apiv1_images(&self, _http_stream: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        response_json(StatusCode::OK, Into::<apiv1::images::Response>::into(&self.index))
            .map_err(|_| Error::Serialize)
    }
}

#[async_trait]
impl ServeHttp for HttpRunnerApp {
    async fn response(&self, http_stream: &mut ServerSession) -> Response<Vec<u8>> {
        let req_parts: &http::request::Parts = http_stream.req_header();
        trace!("{} {}", req_parts.method, req_parts.uri.path());
        let res = match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::GET,  "/api/v1/images") => self.apiv1_images(http_stream).await,
            (Method::POST, path) if path.starts_with(ROUTE_API_V1_RUNI) => self.apiv1_runi(http_stream).await,
            _ => {
                return response_no_body(StatusCode::NOT_FOUND)
            }
        };
        res.unwrap_or_else(|e| e.into())
    }
}

fn main() {
    env_logger::init();
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
    let index = PEImageMultiIndex::from_paths_by_digest_with_slash(&["../ocismall.erofs"]).unwrap();
    let app = HttpRunnerApp {
        pool             : pool,
        index            : index,
        kernel           : cwd.join("../vmlinux").into(),
        initramfs        : cwd.join("../initramfs").into(),
        cloud_hypervisor : cwd.join("../cloud-hypervisor-static").into(),
    };

    assert_file_exists(&app.kernel);
    assert_file_exists(&app.initramfs);
    assert_file_exists(&app.cloud_hypervisor);

    let mut runner_service_http = Service::new("Echo Service HTTP".to_string(), app);
    runner_service_http.add_tcp("127.0.0.1:1234");

    let services: Vec<Box<dyn IService>> = vec![
        Box::new(runner_service_http),
    ];
    my_server.add_services(services);
    my_server.run_forever();
}

use std::path::Path;
fn assert_file_exists<P: AsRef<Path>>(p: P) {
    assert!(p.as_ref().is_file(), "{:?} is not a file", p.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_apiv1_runi_path() {
        parse_
    }
}
