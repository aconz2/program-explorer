use std::time::Duration;
use std::io::{Read,Write};
use std::ffi::OsString;

use pingora_timeout::timeout;
use pingora::services::listening::Service;
use pingora::server::Server;
use pingora::server::configuration::{Opt,ServerConf};
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::ServerSession;

use async_trait::async_trait;
use http;
use http::{Method,Response,StatusCode,header};
use tempfile::NamedTempFile;
use serde_json;
use serde::{Serialize};
use env_logger;
use log::log_enabled;
use clap::Parser;
use once_cell::sync::Lazy;
use prometheus::{register_int_counter,IntCounter};

use peinit;
use perunner::{worker,create_runtime_spec};
use perunner::cloudhypervisor::{CloudHypervisorConfig,round_up_file_to_pmem_size};
use peimage::PEImageMultiIndex;

use peserver::api;
use peserver::api::ContentType;
use peserver::api::v1 as apiv1;
use peserver::util::{
    read_full_server_request_body,
    response_json,response_no_body,response_json_vec,response_pearchivev1
};

static REQ_IMAGES_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("req_images", "Number of images requests").unwrap()
});

static REQ_RUN_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("req_run", "Number of run requests").unwrap()
});

static ERR_CH_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("err_ch", "Number of ch errors").unwrap()
});

// timeout we put on the user's process (after the initial crun process exits)
const RUN_TIMEOUT: Duration      = Duration::from_millis(1000);
// overhead from kernel boot and crun start
const CH_TIMEOUT_EXTRA: Duration = Duration::from_millis(300);

#[derive(Debug,Serialize,Clone)]
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
    OciSpec,
}

#[derive(Serialize)]
struct ErrorBody {
    error: Error,
}

struct HttpRunnerApp {
    pub pool: worker::asynk::Pool,
    pub index: PEImageMultiIndex,
    pub cloud_hypervisor: OsString,
    pub initramfs: OsString,
    pub kernel: OsString,
}

//fn response_with_message(status: StatusCode, message: &str) -> Response<Vec<u8>> {
//    let body: Vec<_> = message.into();
//    Response::builder()
//        .status(status)
//        .header(http::header::CONTENT_LENGTH, body.len())
//        .body(body)
//        .unwrap()
//}

impl Into<StatusCode> for Error {
    fn into(self: Error) -> StatusCode {
        use Error::*;
        match self {
            ReadTimeout => StatusCode::REQUEST_TIMEOUT,
            ReadError |
            NoSuchImage |
            BadContentType |
            BadImagePath |
            OciSpec |
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

// TODO use lazy static for most cmmon responses
impl Into<Response<Vec<u8>>> for Error {
    fn into(self: Error) -> Response<Vec<u8>> {
        // response_no_body(self.into())
        response_json(self.clone().into(), ErrorBody{ error: self }).unwrap()
    }
}

impl HttpRunnerApp {
    async fn apiv1_runi(&self, session: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        REQ_RUN_COUNT.inc();
        let req_parts: &http::request::Parts = session.req_header();

        let uri_path_image = apiv1::runi::parse_path(req_parts.uri.path())
            .ok_or(Error::BadImagePath)?;

        let image_entry = self.index.get(&uri_path_image)
            .ok_or(Error::NoSuchImage)?;

        let content_type = session.req_header()
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|x| x.to_str().ok())
            .and_then(|x| x.try_into().ok())
            .ok_or(Error::BadContentType)?;

        // not using accept header
        let response_format = match content_type {
            ContentType::ApplicationJson => peinit::ResponseFormat::JsonV1,
            ContentType::PeArchiveV1 => peinit::ResponseFormat::PeArchiveV1,
        };

        // TODO this is a timeout on the reading the entire body, session.read_timeout
        let read_timeout = Duration::from_millis(2000);
        // TODO ideally could read this in two parts to send the rest to the file
        let body = timeout(read_timeout, read_full_server_request_body(session, api::MAX_BODY_SIZE))
            .await
            .map_err(|_| Error::ReadTimeout)?
            .map_err(|_| Error::ReadError)?;

        let (body_offset, api_req) = apiv1::runi::parse_request(&body, &content_type)
            .ok_or(Error::BadRequest)?;
        let runtime_spec = create_runtime_spec(&image_entry.image.config, api_req.entrypoint.as_deref(), api_req.cmd.as_deref())
            .map_err(|_| Error::OciSpec)?;

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
            timeout            : RUN_TIMEOUT,
            oci_runtime_config : serde_json::to_string(&runtime_spec).unwrap(),
            stdin              : api_req.stdin,
            strace             : false,
            crun_debug         : false,
            rootfs_dir         : image_entry.image.rootfs.clone(),
            rootfs_kind        : image_entry.rootfs_kind,
            response_format    : response_format,
            kernel_inspect     : false,
        };

        // TODO we should have this io_file crap not in here
        let mut io_file = NamedTempFile::new().map_err(|_| Error::TempfileCreate)?;
        match content_type {
            ContentType::ApplicationJson => {
                // this is blocking, but is going to tmpfs so I don't think its bad to do this?
                peinit::write_io_file_config(io_file.as_file_mut(), &pe_config, 0).map_err(|_| Error::Internal)?;
            }
            ContentType::PeArchiveV1 => {
                // this is blocking but should be fine, right?
                let archive_size: u32 = (body.len() - body_offset).try_into().map_err(|_| Error::Internal)?;
                peinit::write_io_file_config(io_file.as_file_mut(), &pe_config, archive_size).map_err(|_| Error::Internal)?;
                io_file.write_all(&body[body_offset..]).map_err(|_| Error::Internal)?;
            }
        }
        let _ = round_up_file_to_pmem_size(io_file.as_file_mut()).map_err(|_| Error::Internal)?;

        let worker_input = worker::Input {
            id         : 42, // id is useless because we are passing a return channel
            ch_config  : ch_config,
            ch_timeout : RUN_TIMEOUT + CH_TIMEOUT_EXTRA,
            io_file    : io_file,
            rootfs     : image_entry.path.clone().into(),
        };

        let (resp_sender, resp_receiver) = tokio::sync::oneshot::channel();

        () = self.pool.sender()
            .try_send((worker_input, resp_sender))
            .map_err(|_| {
                Error::QueueFull
            })?;

        let mut worker_output = resp_receiver
            .await
            .map_err(|_| Error::WorkerRecv)?
            .map_err(|postmortem| {
                ERR_CH_COUNT.inc();
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

        if log_enabled!(log::Level::Debug) {
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
                peinit::read_io_file_response_bytes(worker_output.io_file.as_file_mut())
                    .map_err(|_| Error::ResponseRead)
                    .map(|(_archive_size, json_bytes)| response_json_vec(StatusCode::OK, json_bytes))
            }
            peinit::ResponseFormat::PeArchiveV1 => {
                peinit::read_io_file_response_archive_bytes(worker_output.io_file.as_file_mut())
                    .map_err(|_| Error::ResponseRead)
                    .map(|response_bytes| response_pearchivev1(StatusCode::OK, response_bytes))
            }
        }
    }

    async fn apiv1_images(&self, _session: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        REQ_IMAGES_COUNT.inc();
        response_json(StatusCode::OK, Into::<apiv1::images::Response>::into(&self.index))
            .map_err(|_| Error::Serialize)
    }
}

#[async_trait]
impl ServeHttp for HttpRunnerApp {
    async fn response(&self, session: &mut ServerSession) -> Response<Vec<u8>> {
        let req_parts: &http::request::Parts = session.req_header();
        //trace!("{} {}", req_parts.method, req_parts.uri.path());
        let res = match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::GET,  apiv1::images::PATH) => self.apiv1_images(session).await,
            (Method::POST, path) if path.starts_with(apiv1::runi::PREFIX) => self.apiv1_runi(session).await,
            _ => {
                return response_no_body(StatusCode::NOT_FOUND)
            }
        };
        res.unwrap_or_else(|e| e.into())
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "../cloud-hypervisor-static")]
    ch: OsString,

    #[arg(long, default_value = "../vmlinux")]
    kernel: OsString,

    #[arg(long, default_value = "../initramfs")]
    initramfs: OsString,

    #[arg(long, default_value = "../ocismall.erofs")]
    index: Vec<OsString>,
}


fn main() {
    env_logger::init();
    let cwd = std::env::current_dir().unwrap();
    let args = Args::parse();

    //let opt = Some(Opt::parse_args());
    let opt = Some(Opt {
        upgrade: false,
        daemon: false,
        nocapture: false,
        test: false,
        conf: None // path to configuration file
    });

    let conf = ServerConf::default();
    //conf.threads = 1;

    //let mut my_server = Server::new(opt).unwrap();
    let mut my_server = Server::new_with_opt_and_conf(opt, conf);
    my_server.bootstrap();

    let pool = worker::asynk::Pool::new(&worker::cpuset(2, 2, 2).unwrap());
    let index = PEImageMultiIndex::from_paths_by_digest_with_colon(&args.index).unwrap();
    let app = HttpRunnerApp {
        pool             : pool,
        index            : index,
        kernel           : cwd.join(args.kernel).into(),
        initramfs        : cwd.join(args.initramfs).into(),
        cloud_hypervisor : cwd.join(args.ch).into(),
    };

    // TODO multiple kernels
    assert_file_exists(&app.kernel);
    assert_file_exists(&app.initramfs);
    assert_file_exists(&app.cloud_hypervisor);

    let mut runner_service_http = Service::new("Program Explorer Worker".to_string(), app);
    runner_service_http.add_tcp("127.0.0.1:1234");

    let mut prometheus_service_http = Service::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6192");

    my_server.add_service(runner_service_http);
    my_server.add_service(prometheus_service_http);

    my_server.run_forever();
}

use std::path::Path;
fn assert_file_exists<P: AsRef<Path>>(p: P) {
    assert!(p.as_ref().is_file(), "{:?} is not a file", p.as_ref());
}
