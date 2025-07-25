use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use pingora::http::RequestHeader;
use pingora::prelude::{timeout, HttpPeer};
use pingora::protocols::http::v1::common::header_value_content_length;
use pingora::protocols::l4::socket::SocketAddr;
use pingora::proxy::{ProxyHttp, Session};
use pingora::server::configuration::{Opt, ServerConf};
use pingora::server::Server;
use pingora::services::background::{background_service, BackgroundService};
use pingora::services::listening::Service;
use pingora::upstreams::peer::Peer;
use pingora::Result;
use pingora_limits::rate::Rate;

use async_trait::async_trait;
use clap::Parser;
use http::{header, Method, StatusCode};
use log::{error, info, warn};
use once_cell::sync::Lazy;
use prometheus::{register_int_counter, IntCounter};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use peserver::api;
use peserver::api::v2 as apiv2;

use peserver::util::premade_responses;
use peserver::util::{read_full_client_response_body, session_ip_id, setup_logs};

static REQ_RUN_COUNT: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("lb_req_run", "Number of run requests").unwrap());

// write_response_header_ref makes a Box::new(x.clone()) internally! I guess it has to but
// maybe there could be a path to take an Arc so that you don't actually have to copy?
// and I wish I could preload the downstream request body so that it is in memory before sending it
// upstream

const TLS_FALSE: bool = false;

// these are the defaults from pingora-limits/src/rate.rs
// TODO understand how to tune these
const HASHES: usize = 4;
const SLOTS: usize = 1024;
static RATE_LIMITER: Lazy<Rate> =
    Lazy::new(|| Rate::new_with_estimator_config(Duration::from_secs(1), HASHES, SLOTS));

type WorkerId = u16;

#[derive(Debug)]
struct Worker {
    peer: HttpPeer,
    // TODO make this dynamic or something
    max_conn: Arc<Semaphore>,
}

impl Worker {
    fn new(peer: HttpPeer, max_conn: usize) -> Self {
        Self {
            peer,
            max_conn: Semaphore::new(max_conn).into(),
        }
    }

    fn address(&self) -> &SocketAddr {
        self.peer.address()
    }
}

// peers could be dynamic in the future, but always have to maintain the same id
struct Workers {
    workers: Vec<Arc<Worker>>,
    image_check_frequency: Duration,
}

impl Workers {
    fn new(workers: Vec<Worker>, image_check_frequency: Duration) -> Option<Self> {
        if workers.is_empty() {
            return None;
        }
        if workers.len() > WorkerId::MAX.into() {
            return None;
        }

        let workers: Vec<_> = workers.into_iter().map(Arc::new).collect();

        Some(Self {
            workers,
            image_check_frequency,
        })
    }

    fn get_worker(&self, id: WorkerId) -> Option<&Arc<Worker>> {
        self.workers.get(id as usize)
    }

    async fn get_max_conn(&self, peer: &HttpPeer) -> Result<usize, Box<pingora::Error>> {
        let connector = pingora::connectors::http::v1::Connector::new(None);
        let (mut session, _) = connector.get_http_session(peer).await?;
        session.read_timeout = Some(Duration::from_secs(5));
        session.write_timeout = Some(Duration::from_secs(5));
        let req = {
            let x = RequestHeader::build(Method::GET, "/api/internal/maxconn".as_bytes(), None)
                .unwrap();
            Box::new(x)
        };

        let _ = session.write_request_header(req).await?;
        let _ = session.read_response().await?;
        let res_parts: &http::response::Parts = session.resp_header().unwrap();
        if res_parts.status != StatusCode::OK {
            error!("got bad response for maxconn {:?}", res_parts);
            return Err(pingora::Error::new(pingora::ErrorType::InternalError));
        }
        let body = read_full_client_response_body(&mut session).await?;
        let s = String::from_utf8_lossy(&body);
        s.parse::<usize>()
            .map_err(|_| pingora::Error::new(pingora::ErrorType::InternalError))
    }
}

#[async_trait]
impl BackgroundService for Workers {
    async fn start(&self, _shutdown: pingora::server::ShutdownWatch) -> () {
        //let mut interval = tokio::time::interval(self.image_check_frequency);
        // TODO do this better
        for (id, worker) in self.workers.iter().enumerate() {
            for _ in 0..20 {
                match self.get_max_conn(&worker.peer).await {
                    Ok(max_conn) => {
                        info!("updating maxconn for worker={} to {}", id, max_conn);
                        worker.max_conn.add_permits(max_conn);
                        break;
                    }
                    Err(_) => {
                        warn!("error getting maxconn for worker={}", id);
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        //loop {
        //    if *shutdown.borrow() {
        //        return;
        //    }
        //
        //    // shouldn't this be a select on the shutdown signal?
        //    interval.tick().await;
        //}
    }
}

pub struct LB {
    workers: Arc<Workers>,
    max_conn: Arc<Semaphore>,
}

pub struct LBCtxInner {
    worker: Arc<Worker>,
    #[allow(dead_code)]
    lb_permit: OwnedSemaphorePermit,
    #[allow(dead_code)]
    worker_permit: OwnedSemaphorePermit,
}

pub struct LBCtx {
    inner: Option<LBCtxInner>,
}

impl LBCtx {
    fn new() -> Self {
        Self { inner: None }
    }
    fn is_some(&self) -> bool {
        self.inner.is_some()
    }
    fn peer(&self) -> Option<HttpPeer> {
        self.inner.as_ref().map(|inner| inner.worker.peer.clone())
    }
    fn replace(&mut self, inner: LBCtxInner) {
        assert!(self.inner.is_none());
        self.inner.replace(inner);
    }
}

impl LB {
    fn new(max_conn: usize, workers: Arc<Workers>) -> Self {
        Self {
            workers,
            max_conn: Semaphore::new(max_conn).into(),
        }
    }

    // Ok(true) means request done, ie we didn't forward to the upstream
    async fn apiv2_runi(&self, session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        REQ_RUN_COUNT.inc();
        let req_parts: &http::request::Parts = session.downstream_session.req_header();

        // TODO here we will parse the arch+os and lookup an appropriate worker
        if apiv2::runi::parse_path(req_parts.uri.path()).is_none() {
            return session
                .downstream_session
                .write_response_header_ref(&premade_responses::BAD_REQUEST)
                .await
                .map(|_| true);
        }

        // if there is no content-length (maybe it is chunked), and the body is too large
        // the worker server will throw an error and that will get propagated back; though it
        // will just be a 500, not 413
        match header_value_content_length(req_parts.headers.get(header::CONTENT_LENGTH)) {
            Some(l) if l > api::MAX_BODY_SIZE => {
                session
                    .downstream_session
                    .write_response_header_ref(&premade_responses::PAYLOAD_TOO_LARGE)
                    .await?;
                return Err(pingora::Error::new(pingora::ErrorType::ReadError));
            }
            _ => {}
        }

        let Some(worker) = self.workers.get_worker(0) else {
            return session
                .downstream_session
                .write_response_header_ref(&premade_responses::INTERNAL_SERVER_ERROR)
                .await
                .map(|_| true);
        };

        match self.get_permits(worker).await {
            Some(ctx_inner) => {
                ctx.replace(ctx_inner);
                Ok(false)
            }
            None => session
                .downstream_session
                .write_response_header_ref(&premade_responses::SERVICE_UNAVAILABLE)
                .await
                .map(|_| true),
        }
    }

    async fn get_permits(&self, worker: &Arc<Worker>) -> Option<LBCtxInner> {
        let lb_permit = self.max_conn.clone().try_acquire_owned().ok()?;
        let worker_permit = {
            match timeout(
                api::MAX_WAIT_TIMEOUT,
                worker.max_conn.clone().acquire_owned(),
            )
            .await
            {
                Ok(Ok(permit)) => permit,
                _ => {
                    // either timeout or error acquiring
                    return None;
                }
            }
        };
        Some(LBCtxInner {
            lb_permit,
            worker_permit,
            worker: worker.clone(),
        })
    }

    fn rate_limit(&self, session: &mut Session, _ctx: &mut LBCtx) -> bool {
        let curr_window_requests = RATE_LIMITER.observe(&session_ip_id(session), 1);
        curr_window_requests > api::MAX_REQ_PER_SEC
    }
}

// LB phases go (from HTTPProxy::proxy_request)
// * early_request_filter
// * downstream_modules
// * request_filter
// * proxy_cache
// * proxy_upstream_filter
// * proxy_to_upstream which calls upstream_peer

#[async_trait]
impl ProxyHttp for LB {
    type CTX = LBCtx;

    fn new_ctx(&self) -> LBCtx {
        LBCtx::new()
    }

    // This function is only called once when the server starts
    // for some reason the default has a downstream response compression builder that is disabled
    //fn init_downstream_modules(&self, modules: &mut HttpModules) {
    //    modules.add_module(ResponseCompressionBuilder::enable(5));
    //}

    // Ok(true) means request is done
    async fn request_filter(&self, session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        if self.rate_limit(session, ctx) {
            return session
                .downstream_session
                .write_response_header_ref(&premade_responses::TOO_MANY_REQUESTS)
                .await
                .map(|_| true);
        }

        let req_parts: &http::request::Parts = session.downstream_session.req_header();

        let ret = match (&req_parts.method, req_parts.uri.path()) {
            (&Method::POST, path) if path.starts_with(apiv2::runi::PREFIX) => {
                self.apiv2_runi(session, ctx).await
            }
            _ => session
                .downstream_session
                .write_response_header_ref(&premade_responses::NOT_FOUND)
                .await
                .map(|_| true),
        };
        ret
    }

    //async fn request_body_filter(&self, session: &mut Session, body: &mut Option<Bytes>, _end_of_stream: bool, ctx: &mut LBCtx) -> Result<()> {
    //    if ctx.add_body_len(body.as_ref().map(|b| b.len())) > api::MAX_BODY_SIZE {
    //        info!("content length too big, terminating");
    //        session.shutdown().await;
    //    }
    //    Ok(())
    //}

    // is it okay to send request upstream?
    async fn proxy_upstream_filter(&self, _session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        Ok(ctx.is_some())
    }

    // what peer should we send the request to?
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut LBCtx,
    ) -> Result<Box<HttpPeer>> {
        // should be Some because proxy_upstream_filter should filter those which are None
        ctx.peer()
            .map(Box::new)
            .ok_or_else(|| pingora::Error::new(pingora::ErrorType::ConnectProxyFailure))
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long)]
    tcp: Option<String>,

    #[arg(long)]
    uds: Option<String>,

    //#[arg(long, default_value="127.0.0.1:6192")]
    #[arg(long)]
    prom: Option<String>,

    #[arg(long)]
    worker: Vec<String>,
}

#[derive(Debug)]
enum PeerParseError {
    UdsError,
    BadFmt,
    BadKind,
}

fn parse_peers(args: &[String]) -> Result<Vec<Worker>, PeerParseError> {
    use PeerParseError::*;
    let mut ret = vec![];
    for arg in args {
        let (kind, addr) = arg.split_once(':').ok_or(BadFmt)?;
        let worker = match kind {
            "uds" => Worker::new(
                HttpPeer::new_uds(addr, TLS_FALSE, "".to_string()).map_err(|_| UdsError)?,
                0,
            ),
            "tcp" => Worker::new(HttpPeer::new(addr, TLS_FALSE, "".to_string()), 0),
            _ => {
                return Err(BadKind);
            }
        };
        ret.push(worker);
    }
    Ok(ret)
}

fn main() {
    setup_logs();

    let args = Args::parse();

    if args.tcp.is_none() && args.uds.is_none() {
        println!("--tcp or --uds must be provided");
        std::process::exit(1);
    }

    let opt = Some(Opt {
        upgrade: false,
        daemon: false,
        nocapture: false,
        test: false,
        conf: None, // path to configuration file
    });
    let conf = ServerConf::default();

    let mut my_server = Server::new_with_opt_and_conf(opt, conf);
    info!("config {:#?}", my_server.configuration);
    my_server.bootstrap();

    let peers = parse_peers(&args.worker).expect("no peers");
    for peer in &peers {
        info!("peer {:?}", peer.address());
    }

    if peers.is_empty() {
        println!("no worker peers, add with --worker");
        std::process::exit(1);
    }

    let image_check_frequency = Duration::from_secs(120);
    let workers = Workers::new(peers, image_check_frequency).unwrap();

    for (worker_id, worker) in workers.workers.iter().enumerate() {
        info!("worker {} {:?}", worker_id, Arc::as_ptr(worker));
    }
    let workers_background = background_service("workers", workers);
    let workers = workers_background.task();

    let lb_maxconn = 1024;
    let lb = LB::new(lb_maxconn, workers);
    let mut lb_service = pingora::proxy::http_proxy_service(&my_server.configuration, lb);

    if let Some(addr) = args.tcp {
        info!("listening on tcp {}", addr);
        lb_service.add_tcp(&addr);
    }
    if let Some(addr) = args.uds {
        info!("listening on uds {}", addr);
        lb_service.add_uds(&addr, Some(Permissions::from_mode(0o600)));
    }

    if let Some(addr) = args.prom {
        let mut prometheus_service_http = Service::prometheus_http_service();
        prometheus_service_http.add_tcp(&addr);
        my_server.add_service(prometheus_service_http);
    }

    //let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    //let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
    //
    //let mut tls_settings =
    //    pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    //tls_settings.enable_h2();
    //lb.add_tls_with_settings("0.0.0.0:6189", None, tls_settings);

    my_server.add_service(lb_service);
    my_server.add_service(workers_background);

    my_server.run_forever();
}
