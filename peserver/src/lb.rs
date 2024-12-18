use std::sync::Arc;
use std::time::Duration;
use std::collections::{BTreeMap};

use pingora::prelude::{timeout,HttpPeer};
use pingora::http::{RequestHeader,ResponseHeader};
use pingora::services::background::{background_service,BackgroundService};
use pingora::server::configuration::{Opt,ServerConf};
use pingora::server::Server;
use pingora::Result;
use pingora::proxy::{ProxyHttp, Session};
use pingora_limits::rate::Rate;
use pingora::protocols::http::v1::common::header_value_content_length;
use pingora::services::listening::Service;

use async_trait::async_trait;
use env_logger;
use bytes::Bytes;
use http::{Method,StatusCode,header};
use arc_swap::ArcSwap;
use log::{error,info};
use serde_json;
use once_cell::sync::Lazy;
use tokio::sync::{Semaphore,OwnedSemaphorePermit};
use prometheus::{register_int_counter,IntCounter};

use peserver::api::v1 as apiv1;
use peserver::api;

use peserver::util::{read_full_client_response_body,session_ip_id,etag};
use peserver::util::premade_responses;

static REQ_IMAGES_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("lb_req_images", "Number of images requests").unwrap()
});

static REQ_IMAGES_CACHE_HIT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("lb_req_images_cache_hit", "Number of images requests cache hit").unwrap()
});

static REQ_RUN_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("lb_req_run", "Number of run requests").unwrap()
});

// write_response_header_ref makes a Box::new(x.clone()) internally! I guess it has to but
// maybe there could be a path to take an Arc so that you don't actually have to copy?
// and I wish I could preload the downstream request body so that it is in memory before sending it
// upstream

const TLS_FALSE: bool = false;

// these are the defaults from pingora-limits/src/rate.rs
// TODO understand how to tune these
const HASHES: usize = 4;
const SLOTS: usize = 1024;
static RATE_LIMITER: Lazy<Rate> = Lazy::new(|| Rate::new_with_estimator_config(Duration::from_secs(1), HASHES, SLOTS));

type WorkerId = u16;

// TODO images probably needs the images kept per worker, image_map might have a list of workerids
// premade_json should store a merged view of all images
pub struct ImageData {
    image_map: BTreeMap<String, WorkerId>,
    premade_json: Bytes,
    premade_json_response_header: ResponseHeader,
    premade_json_etag: String,
}

impl ImageData {
    fn from_parts(image_map: BTreeMap<String, WorkerId>,
                  premade_json: Bytes,
                  ) -> Self {
        let premade_json_etag = etag(&premade_json);
        let premade_json_response_header = {
            let mut x = ResponseHeader::build(200, Some(3)).unwrap();
            x.insert_header(header::CONTENT_TYPE, "application/json").unwrap();
            x.insert_header(header::CONTENT_LENGTH, premade_json.len()).unwrap();
            x.insert_header(header::CACHE_CONTROL, "max-age=3600").unwrap();
            x.insert_header(header::ETAG, premade_json_etag.clone()).unwrap();
            x
        };
        Self { image_map, premade_json, premade_json_response_header, premade_json_etag }
    }
    fn new() -> Self {
        let premade_json = serde_json::to_vec(&apiv1::images::Response{images: vec![]}).unwrap();
        Self::from_parts(
            BTreeMap::new(),
            Bytes::from(premade_json),
        )
    }

    fn update(&self, id: WorkerId, images: Vec<apiv1::images::Image>) -> Self {
        let map: BTreeMap<_, _> = {
            images
                .iter()
                .map(|img| (img.info.digest.clone(), id))
                .collect()
        };
        let premade_json = serde_json::to_vec(&apiv1::images::Response{images}).unwrap();
        Self::from_parts(map, premade_json.into())
    }
}

pub struct Worker {
    peer: HttpPeer,
    max_conn: Arc<Semaphore>,
}

impl Worker {
    fn new(peer: HttpPeer, max_conn: usize) -> Self {
        Self { peer, max_conn: Semaphore::new(max_conn).into() }
    }
}

// peers could be dynamic in the future, but always have to maintain the same id
pub struct Workers {
    workers: Vec<Arc<Worker>>,
    data: ArcSwap<ImageData>,
    image_check_frequency: Duration,
}

impl Workers {
    fn new(workers: Vec<Worker>, image_check_frequency: Duration) -> Option<Self> {
        if workers.is_empty() { return None; }
        if workers.len() > WorkerId::MAX.into() { return None; }

        let workers: Vec<_> = workers.into_iter().map(|x| Arc::new(x)).collect();

        Some(Self {
            workers,
            data: ArcSwap::from_pointee(ImageData::new()),
            image_check_frequency,
        })
    }

    fn get_worker(&self, id: WorkerId) -> Option<&Arc<Worker>> { self.workers.get(id as usize) }

    fn update_data(&self, worker_id: WorkerId, resp: apiv1::images::Response) {
        self.data.store(self.data.load().update(worker_id, resp.images).into());
    }

    async fn do_update(&self, worker_id: WorkerId, peer: &HttpPeer) -> Result<(), Box<pingora::Error>> {
        let connector = pingora::connectors::http::v1::Connector::new(None);
        let (mut session, _) = connector.get_http_session(peer).await?;
        session.read_timeout = Some(Duration::from_secs(5));
        session.write_timeout = Some(Duration::from_secs(5));

        // TODO: maybe use proper cache headers to only update when changed
        let req = {
            let x = RequestHeader::build(Method::GET, apiv1::images::PATH.as_bytes(), None).unwrap();
            Box::new(x)
        };
        let _ = session.write_request_header(req).await?;
        let _ = session.read_response().await?;
        let res_parts: &http::response::Parts = session.resp_header().unwrap();
        if res_parts.status != StatusCode::OK {
            error!("got bad response for images {:?}", res_parts);
            return Err(pingora::Error::new(pingora::ErrorType::InternalError));
        }

        let body = read_full_client_response_body(&mut session).await?;
        let resp: apiv1::images::Response = serde_json::from_slice(&body)
            .map_err(|_| pingora::Error::new(pingora::ErrorType::InternalError))?;
        let n_images = resp.images.len();

        self.update_data(worker_id, resp);

        info!("updated images for backend {}, {} images", peer, n_images);
        Ok(())
    }
}

#[async_trait]
impl BackgroundService for Workers {
    async fn start(&self, shutdown: pingora::server::ShutdownWatch) -> () {
        let mut interval = tokio::time::interval(self.image_check_frequency);
        loop {
            if *shutdown.borrow() {
                return;
            }

            // shouldn't this be a select on the shutdown signal?
            interval.tick().await;

            // TODO in parallel or something (if dynamic, hard to spawn task per)
            for (id, worker) in self.workers.iter().enumerate() {
                match self.do_update(id.try_into().unwrap(), &worker.peer).await {
                    Ok(()) => {}
                    Err(e) => { error!("error getting images for peer {} {:?} {:?}", id, worker.peer, e); }
                }
            }
        }
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
    fn new() -> Self { Self{ inner: None } }
    fn is_some(&self) -> bool { self.inner.is_some() }
    fn peer(&self) -> Option<HttpPeer> { self.inner.as_ref().map(|inner| inner.worker.peer.clone()) }
    fn replace(&mut self, inner: LBCtxInner) {
        assert!(self.inner.is_none());
        self.inner.replace(inner.into());
    }
}

impl LB {
    fn new(max_conn: usize, workers: Arc<Workers>) -> Self {
        Self { workers, max_conn: Semaphore::new(max_conn).into() }
    }

    async fn apiv1_images(&self, session: &mut Session, _ctx: &mut LBCtx) -> Result<()> {
        REQ_IMAGES_COUNT.inc();
        session.set_write_timeout(api::DOWNSTREAM_WRITE_TIMEOUT);
        let downstream_session = &mut session.downstream_session;
        let data = self.workers.data.load();
        let req_parts: &http::request::Parts = downstream_session.req_header();
        match req_parts.headers.get(header::IF_NONE_MATCH) {
            Some(etag) if etag.as_bytes() == data.premade_json_etag.as_bytes() => {
                REQ_IMAGES_CACHE_HIT.inc();
                return session.downstream_session
                    .write_response_header_ref(&*premade_responses::NOT_MODIFIED)
                    .await
                    .map(|_| ())
                }
            _ => {}
        }
        // NOTE these skip any filter modules
        downstream_session.write_response_header_ref(&data.premade_json_response_header).await?;
        downstream_session.write_response_body(data.premade_json.clone(), true).await?;
        Ok(())
    }

    // Ok(true) means request done, ie the image was missed
    async fn apiv1_runi(&self, session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        REQ_RUN_COUNT.inc();
        let req_parts: &http::request::Parts = session.downstream_session.req_header();

        // if there is no content-length (maybe it is chunked), and the body is too large
        // the worker server will throw an error and that will get propagated back; though it
        // will just be a 500, not 413
        match header_value_content_length(req_parts.headers.get(header::CONTENT_LENGTH)) {
            Some(l) if l > api::MAX_BODY_SIZE => {
                session.downstream_session
                    .write_response_header_ref(&*premade_responses::PAYLOAD_TOO_LARGE)
                    .await?;
                return Err(pingora::Error::new(pingora::ErrorType::ReadError).into())
            }
            _ => {}
        }

        let uri_path_image = apiv1::runi::parse_path(req_parts.uri.path());

        let image_map = &self.workers.data.load().image_map;

        let worker = uri_path_image
            .and_then(|image_id| image_map.get(image_id).map(|x| *x))
            .and_then(|worker_id| self.workers.get_worker(worker_id));

        let worker = match worker {
            Some(worker) => worker,
            None => {
                return session.downstream_session
                    .write_response_header_ref(&*premade_responses::NOT_FOUND)
                    .await
                    .map(|_| true);
            }
        };

        match self.get_permits(worker).await {
            Some(ctx_inner) => {
                ctx.replace(ctx_inner);
                Ok(false)
            }
            None => {
                session.downstream_session
                    .write_response_header_ref(&*premade_responses::SERVICE_UNAVAILABLE)
                    .await
                    .map(|_| true)
            }
        }
    }

    async fn get_permits(&self, worker: &Arc<Worker>) -> Option<LBCtxInner> {
        let lb_permit = self.max_conn.clone().try_acquire_owned().ok()?;
        let worker_permit = {
            match timeout(api::MAX_WAIT_TIMEOUT, worker.max_conn.clone().acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                _ => {  // either timeout or error acquiring
                    return None;
                }
            }
        };
        Some(LBCtxInner{ lb_permit, worker_permit, worker: worker.clone() })
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

    fn new_ctx(&self) -> LBCtx { LBCtx::new() }

    // This function is only called once when the server starts
    // for some reason the default has a downstream response compression builder that is disabled
    //fn init_downstream_modules(&self, modules: &mut HttpModules) {
    //    modules.add_module(ResponseCompressionBuilder::enable(5));
    //}

    // Ok(true) means request is done
    async fn request_filter(&self, session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        if self.rate_limit(session, ctx) {
            return session.downstream_session
                .write_response_header_ref(&*premade_responses::TOO_MANY_REQUESTS)
                .await
                .map(|_| true)
        }

        let req_parts: &http::request::Parts = session.downstream_session.req_header();

        let ret = match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::GET,  apiv1::images::PATH) => self.apiv1_images(session, ctx).await.map(|_| true),
            (Method::POST, path) if path.starts_with(apiv1::runi::PREFIX) => self.apiv1_runi(session, ctx).await,
            _ => {
                session.downstream_session
                    .write_response_header_ref(&*premade_responses::NOT_FOUND)
                    .await
                    .map(|_| true)
            }
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
    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut LBCtx) -> Result<Box<HttpPeer>> {
        // should be Some because proxy_upstream_filter should filter those which are None
        ctx.peer()
           .map(Box::new)
           .ok_or_else(|| pingora::Error::new(pingora::ErrorType::ConnectProxyFailure))
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
    let conf = ServerConf::default();

    let mut my_server = Server::new_with_opt_and_conf(opt, conf);
    println!("server config {:#?}", my_server.configuration);
    my_server.bootstrap();

    let peers = vec![
        Worker::new(HttpPeer::new("127.0.0.1:1234", TLS_FALSE, "".to_string()), 4),
    ];

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
    lb_service.add_tcp("127.0.0.1:6188");

    let mut prometheus_service_http = Service::prometheus_http_service();
    // TODO This has to be on a different port than main
    prometheus_service_http.add_tcp("127.0.0.1:6192");

    //let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    //let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
    //
    //let mut tls_settings =
    //    pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    //tls_settings.enable_h2();
    //lb.add_tls_with_settings("0.0.0.0:6189", None, tls_settings);

    my_server.add_service(lb_service);
    my_server.add_service(workers_background);
    my_server.add_service(prometheus_service_http);

    my_server.run_forever();
}
