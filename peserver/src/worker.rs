use std::time::Duration;
use std::io::{Read,Write};
use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

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
use log::{info,error,log_enabled};
use clap::Parser;
use once_cell::sync::Lazy;
use prometheus::{register_int_counter,IntCounter};
use nix;

use peinit;
use perunner::{worker,create_runtime_spec};
use perunner::cloudhypervisor::{CloudHypervisorConfig,round_up_file_to_pmem_size,ChLogLevel};
use peimage::PEImageMultiIndex;

use peserver::api;
use peserver::api::ContentType;
use peserver::api::v1 as apiv1;
use peserver::util::{
    read_full_server_request_body,
    response_json,response_no_body,response_json_vec,response_pearchivev1,response_string,setup_logs,
};

static REQ_IMAGES_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("worker_req_images", "Worker number of images requests").unwrap()
});

static REQ_RUN_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("worker_req_run", "Worker number of run requests").unwrap()
});

static ERR_CH_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("worker_err_ch", "Worker number of ch errors").unwrap()
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
    pub max_conn: usize,
    pub index: PEImageMultiIndex,
    pub cloud_hypervisor: OsString,
    pub initramfs: OsString,
    pub kernel: OsString,
    pub ch_console: bool,
    pub ch_log_level: Option<ChLogLevel>,
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

        let runtime_spec = create_runtime_spec(&image_entry.image.config, api_req.entrypoint.as_deref(), api_req.cmd.as_deref(), api_req.env.as_deref())
            .map_err(|_| Error::OciSpec)?;

        // TODO plumb in an arg
        let ch_config = CloudHypervisorConfig {
            bin           : self.cloud_hypervisor.clone(),
            kernel        : self.kernel.clone(),
            initramfs     : self.initramfs.clone(),
            log_level     : self.ch_log_level.clone(),
            console       : self.ch_console,
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
                error!("worker error {:?}", postmortem.error);
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

    async fn api_internal_max_conn(&self, _session: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        Ok(response_string(StatusCode::OK, &format!("{}", self.max_conn)))
    }
}

#[async_trait]
impl ServeHttp for HttpRunnerApp {
    async fn response(&self, session: &mut ServerSession) -> Response<Vec<u8>> {
        let req_parts: &http::request::Parts = session.req_header();
        //trace!("{} {}", req_parts.method, req_parts.uri.path());
        let res = match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::GET,  "/api/internal/maxconn") => self.api_internal_max_conn(session).await,
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
    // idk these are bad defaults
    #[arg(long, default_value = "../cloud-hypervisor-static")]
    ch: OsString,

    #[arg(long, default_value = "../vmlinux")]
    kernel: OsString,

    #[arg(long, default_value = "../target/debug/initramfs")]
    initramfs: OsString,

    #[arg(long)]
    server_cpuset: Option<String>,

    // can either be
    // 1) offset:num_workers:cores_per_worker
    //   apply an exclusive mask
    // 2) begin-end
    //   apply the same mask to end-begin+1 workers
    #[arg(long)]
    worker_cpuset: Option<String>,

    #[arg(long)]
    tcp: Option<String>,

    #[arg(long)]
    uds: Option<String>,

    //#[arg(long, default_value="127.0.0.1:6193")]
    #[arg(long)]
    prom: Option<String>,

    #[arg(long, default_value="false")]
    ch_console: bool,

    #[arg(long)]
    ch_log_level: Option<String>,

    #[arg(long)]
    index: Vec<OsString>,

    #[arg(long)]
    index_dir: Vec<OsString>,
}

fn parse_cpuset_colon(x: &str) -> Option<(usize, usize, usize)> {
    let mut parts = x.split(":");
    let a = parts.next()?.parse::<usize>().ok()?;
    let b = parts.next()?.parse::<usize>().ok()?;
    let c = parts.next()?.parse::<usize>().ok()?;
    Some((a, b, c))
}

fn parse_cpuset_range(x: &str) -> Option<(usize, Option<usize>)> {
    let mut parts = x.split("-");
    let a = parts.next()?.parse::<usize>().ok()?;
    // isn't this right?
    //let b = parts.next().map(|x| x.parse::<usize>()).transpose().ok()?;
    let b = match parts.next() {
        Some("") | None => None,
        Some(x) => Some(x.parse::<usize>().ok()?),
    };
    Some((a, b))
}

fn main() {
    setup_logs();
    let cwd = std::env::current_dir().unwrap();
    let args = Args::parse();

    if args.tcp.is_none() && args.uds.is_none() {
        error!("--tcp or --uds must be provided");
        std::process::exit(1);
    }

    let opt = Some(Opt {
        upgrade: false,
        daemon: false,
        nocapture: false,
        test: false,
        conf: None // path to configuration file
    });

    let conf = ServerConf::default();
    let mut my_server = Server::new_with_opt_and_conf(opt, conf);
    my_server.bootstrap();
    info!("config {:#?}", my_server.configuration);

    // TODO so we leave the server process running with cpuset given by systemd/taskset
    // so the server is not actually isolated
    // so really I think we want to pass which cpus we should use or maybe parse from isolcpus in
    // the kernel cmdline?
    let server_cpuset = {
        if let Some(cpuspec) = args.server_cpuset {
            let (begin, end) = parse_cpuset_range(&cpuspec).unwrap();
            worker::cpuset_range(begin, end).unwrap()
        } else {
            worker::cpuset_range(0, Some(3)).unwrap()
        }
    };
    let worker_cpuset = {
        if let Some(cpuspec) = args.worker_cpuset {
            if cpuspec.contains(":") {
                let (offset, workers, cores_per) = parse_cpuset_colon(&cpuspec).unwrap();
                worker::cpuset(offset, workers, cores_per).unwrap()
            } else {
                let (begin, end) = parse_cpuset_range(&cpuspec).unwrap();
                worker::cpuset_replicate(&worker::cpuset_range(begin, end).unwrap())
            }
        } else {
            worker::cpuset_replicate(&worker::cpuset_range(4, None).unwrap())
        }
    };

    let pool = worker::asynk::Pool::new(&worker_cpuset);
    info!("using {} workers", pool.len());

    nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &server_cpuset).unwrap();

    let index = {
        let mut index = PEImageMultiIndex::from_paths_by_digest_with_colon(&args.index).unwrap();
        for dir in args.index_dir {
            index.add_dir(dir).unwrap();
        }
        if index.is_empty() {
            error!("index is empty, no images to run");
            std::process::exit(1);
        }
        for (k, v) in index.map() {
            info!("loaded image {} from {:?}: {}", k, v.path, v.image.id.name());
        }
        index
    };
    let max_conn = pool.len() * 2;  // TODO is this a good amount?
    let app = HttpRunnerApp {
        pool             : pool,
        max_conn         : max_conn,
        index            : index,
        // NOTE: these files are opened/passed as paths into cloud hypervisor so changes will
        // get picked up, which may not be what we want. currently ch doesn't support passing
        // as fd= otherwise we could open them here and be sure things didn't change. but it is a
        // bit of a toss up whether it is nicer to just have a new file get picked up on the next
        // run
        // we join with cwd but if you provide an abspath it will be abs
        kernel           : cwd.join(args.kernel).into(),
        initramfs        : cwd.join(args.initramfs).into(),
        cloud_hypervisor : cwd.join(args.ch).into(),

        ch_console:   args.ch_console,
        ch_log_level: args.ch_log_level.map(|x| x.as_str().try_into().unwrap()),
    };

    assert_file_exists(&app.kernel);
    assert_file_exists(&app.initramfs);
    assert_file_exists(&app.cloud_hypervisor);

    let mut runner_service_http = Service::new("Program Explorer Worker".to_string(), app);
    if let Some(addr) = args.tcp {
        info!("listening on tcp {}", addr);
        runner_service_http.add_tcp(&addr);
    }
    if let Some(addr) = args.uds {
        info!("listening on uds {}", addr);
        runner_service_http.add_uds(&addr, Some(Permissions::from_mode(0o600)));
    }

    // ugh i don't think prom can scrape a uds...
    if let Some(addr) = args.prom {
        let mut prometheus_service_http = Service::prometheus_http_service();
        prometheus_service_http.add_tcp(&addr);
        my_server.add_service(prometheus_service_http);
    }

    my_server.add_service(runner_service_http);

    my_server.run_forever();
}

fn assert_file_exists<P: AsRef<Path>>(p: P) {
    assert!(p.as_ref().is_file(), "{:?} is not a file", p.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_cpuset_range_good() {
        assert_eq!(Some((4, Some(8))), parse_cpuset_range("4-8"));
        assert_eq!(Some((4, None)), parse_cpuset_range("4-"));
    }
}
