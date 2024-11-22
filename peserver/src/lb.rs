use std::sync::Arc;
use std::time::Duration;
use std::collections::BTreeMap;

use pingora::prelude::{timeout,RequestHeader};
use pingora::services::background::{background_service,BackgroundService};
use pingora::server::configuration::Opt;
use pingora::server::Server;
use pingora::upstreams::peer::HttpPeer;
use pingora::Result;
//use pingora::lb::{health_check, selection::RoundRobin, LoadBalancer};
use pingora::proxy::{ProxyHttp, Session};
use pingora::http::{ResponseHeader};
use pingora_limits::rate::Rate;

use async_trait::async_trait;
use env_logger;
use bytes::Bytes;
use http::{Method,StatusCode};
use arc_swap::ArcSwap;
use log::{error,info};
use serde_json;
use once_cell::sync::Lazy;
use tokio::sync::{Semaphore,OwnedSemaphorePermit};

use peserver::api::v1 as apiv1;
use peserver::api::premade_errors;
use peserver::api;

mod util;
use util::read_full_body;

// pingora has been mostly fine, I don't know why the caching logic is so baked into their proxy
// though, it makes it a bit complicated to read
// argh: write_response_header_ref makes a Box::new(x.clone()) internally! I guess it has to but
// maybe there could be a path to take an Arc so that you don't actually have to copy?

const TLS_FALSE: bool = false;

// these are the defaults from pingora-limits/src/rate.rs
// TODO understand how to tune these
const HASHES: usize = 4;
const SLOTS: usize = 1024;
static RATE_LIMITER: Lazy<Rate> = Lazy::new(|| Rate::new_with_estimator_config(Duration::from_secs(1), HASHES, SLOTS));

// TODO images probably needs the images kept per worker, image_map might have a list of workerids
// premade_json should store a merged view of all images
pub struct ImageData {
    image_map: BTreeMap<String, WorkerId>,
    premade_json: Bytes,
    premade_json_response_header: ResponseHeader,
}

impl ImageData {
    fn from_parts(image_map: BTreeMap<String, WorkerId>,
                  premade_json: Bytes,
                  ) -> Self {
        let premade_json_response_header = api::make_json_response_header(premade_json.len());
        Self { image_map, premade_json, premade_json_response_header }
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

type WorkerId = u16;

pub struct Worker {
    peer: HttpPeer,
    max_conn: Arc<Semaphore>,
}

const _EXPECTED_ARC_WORKER_SIZE: usize = 8;
const _TEST_ARC_WORKER_SIZE: [u8; _EXPECTED_ARC_WORKER_SIZE] = [0; std::mem::size_of::<Arc<Worker>>()];

// peers could be dynamic in the future, but always have to maintain the same id
pub struct Images {
    peers: Vec<HttpPeer>,
    workers: Vec<Arc<Worker>>,
    //upstreams: Arc<LoadBalancer<RoundRobin>>,
    data: ArcSwap<ImageData>,
    image_check_frequency: Duration,
}

impl Images {
    fn new(workers: Vec<Worker>, image_check_frequency: Duration) -> Option<Self> {
        if workers.is_empty() { return None; }
        if workers.len() > WorkerId::MAX.into() { return None; }

        let peers: Vec<_> = workers.iter().map(|x| x.peer.clone()).collect();
        let workers: Vec<_> = workers.into_iter().map(|x| Arc::new(x)).collect();

        Some(Self {
            peers,
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

        let body = read_full_body(&mut session).await?;
        let resp: apiv1::images::Response = serde_json::from_slice(&body)
            .map_err(|_| pingora::Error::new(pingora::ErrorType::InternalError))?;
        let n_images = resp.images.len();

        self.update_data(worker_id, resp);

        info!("updated images for backend {}, {} images", peer, n_images);
        Ok(())
    }
}

#[async_trait]
impl BackgroundService for Images {
    async fn start(&self, shutdown: pingora::server::ShutdownWatch) -> () {
        let mut interval = tokio::time::interval(self.image_check_frequency);
        loop {
            if *shutdown.borrow() {
                return;
            }

            // shouldn't this be a select on the shutdown signal?
            interval.tick().await;

            // TODO in parallel or something (if dynamic, hard to spawn task per)
            for (id, peer) in self.peers.iter().enumerate() {
                match self.do_update(id.try_into().unwrap(), peer).await {
                    Ok(()) => {}
                    Err(e) => { error!("error getting images for peer {} {:?} {:?}", id, peer, e); }
                }
            }
        }
    }
}

pub struct LB {
    images: Arc<Images>,
    max_conn: Arc<Semaphore>,
}

pub struct LBCtxInner {
    worker: Arc<Worker>,
    #[allow(dead_code)]
    lb_permit: OwnedSemaphorePermit,
    #[allow(dead_code)]
    worker_permit: OwnedSemaphorePermit,
}

pub struct LBCtx(Option<LBCtxInner>);

impl LBCtx {
    fn new() -> Self { Self(None) }
    fn is_some(&self) -> bool { self.0.is_some() }
    //fn map<U, F: FnOnce(&LBCtxInner) -> U>(&self, f: F) -> Option<U> { self.0.as_ref().map(f) }
    fn peer(&self) -> Option<HttpPeer> { self.0.as_ref().map(|inner| inner.worker.peer.clone()) }
    fn replace(&mut self, inner: LBCtxInner) {
        assert!(self.0.is_none());
        self.0.replace(inner.into());
    }
}

//impl Drop for LBCtx {
//    fn drop(&mut self) {
//        info!("dropping lbctx");
//    }
//}

impl LB {
    fn new(max_conn: usize, images: Arc<Images>) -> Self {
        Self { images, max_conn: Semaphore::new(max_conn).into() }
    }

    async fn apiv1_images(&self, session: &mut Session, _ctx: &mut LBCtx) -> Result<()> {
        session.set_write_timeout(api::DOWNSTREAM_WRITE_TIMEOUT);
        let downstream_session = &mut session.downstream_session;
        let data = self.images.data.load();
        // NOTE these skip any filter modules
        downstream_session.write_response_header_ref(&data.premade_json_response_header).await?;
        downstream_session.write_response_body(data.premade_json.clone(), true).await?;
        Ok(())
    }

    // Ok(true) means request done, ie the image was missed
    async fn apiv1_runi(&self, session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        let req_parts: &http::request::Parts = session.downstream_session.req_header();
        let uri_path_image = apiv1::runi::parse_path(req_parts.uri.path());

        let image_map = &self.images.data.load().image_map;

        let worker = uri_path_image
            .and_then(|image_id| image_map.get(image_id).map(|x| *x))
            .and_then(|worker_id| self.images.get_worker(worker_id).map(|x| (worker_id, x)));

        match worker {
            Some((_worker_id, worker)) => {
                // TODO maybe both these semaphores should be sharded and then picked by hash
                // of user or something
                let lb_permit = {
                    match self.max_conn.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            return session.downstream_session
                                .write_response_header_ref(&*premade_errors::SERVICE_UNAVAILABLE)
                                .await
                                .map(|_| true)
                        }
                    }
                };
                let worker_permit = {
                    match timeout(
                        api::MAX_WAIT_TIMEOUT,
                        worker.max_conn.clone().acquire_owned()
                    ).await
                        {
                        Ok(Ok(permit)) => permit,
                        _ => {  // either timeout or error acquiring
                            return session.downstream_session
                                .write_response_header_ref(&*premade_errors::SERVICE_UNAVAILABLE)
                                .await
                                .map(|_| true)
                        }
                    }
                };
                ctx.replace(LBCtxInner{ lb_permit, worker_permit, worker: worker.clone() });
                Ok(false)
            }
            None => {
                session.downstream_session
                    .write_response_header_ref(&*premade_errors::NOT_FOUND)
                    .await
                    .map(|_| true)
            }
        }
    }

    fn rate_limit(&self, session: &mut Session, _ctx: &mut LBCtx) -> bool {
        let ip = session
            .client_addr()
            .and_then(|x| x.as_inet())
            .map(|x| x.ip());
        let curr_window_requests = match ip {
            Some(ip) => RATE_LIMITER.observe(&ip, 1),
            None => RATE_LIMITER.observe(&42, 1),
        };
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

    // Ok(true) means request is done
    async fn request_filter(&self, session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        if self.rate_limit(session, ctx) {
            return session.downstream_session
                .write_response_header_ref(&*premade_errors::TOO_MANY_REQUESTS)
                .await
                .map(|_| true)
        }

        let req_parts: &http::request::Parts = session.downstream_session.req_header();

        let ret = match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::GET,  apiv1::images::PATH) => self.apiv1_images(session, ctx).await.map(|_| true),
            (Method::POST, path) if path.starts_with(apiv1::runi::PREFIX) => self.apiv1_runi(session, ctx).await,
            _ => {
                session.downstream_session
                    .write_response_header_ref(&*premade_errors::NOT_FOUND)
                    .await
                    .map(|_| true)
            }
        };
        use log::trace;
        trace!("request_filter is returning");
        ret
    }

    async fn proxy_upstream_filter(&self, _session: &mut Session, ctx: &mut LBCtx) -> Result<bool> {
        Ok(ctx.is_some())
    }

    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut LBCtx) -> Result<Box<HttpPeer>> {
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

    let mut my_server = Server::new(opt).unwrap();
    println!("server config {:#?}", my_server.configuration);
    my_server.bootstrap();

    //let mut upstreams =
    //    LoadBalancer::try_from_iter(["127.0.0.1:1234"]).unwrap();
    //
    //assert!(upstreams.backends().get_backend().len() == 1, "only one backend supported right now");
    //
    //upstreams.set_health_check(health_check::TcpHealthCheck::new());
    //upstreams.health_check_frequency = Some(Duration::from_secs(10));
    //
    //let upstreams_background = background_service("health check", upstreams);

    //let upstreams = lb_background.task();
    let peers = vec![
        Worker {
            peer: HttpPeer::new("127.0.0.1:1234", TLS_FALSE, "".to_string()),
            max_conn: Semaphore::new(4).into(),
        }
    ];

    let image_check_frequency = Duration::from_secs(120);
    let images = Images::new(peers, image_check_frequency).unwrap();

    for (worker_id, worker) in images.workers.iter().enumerate() {
        info!("worker {} {:?}", worker_id, Arc::as_ptr(worker));
    }
    let images_background = background_service("images", images);
    let images = images_background.task();

    let lb_maxconn = 1024;
    let lb = LB::new(lb_maxconn, images);
    let mut lb_service = pingora::proxy::http_proxy_service(&my_server.configuration, lb);
    lb_service.add_tcp("127.0.0.1:6188");

    //let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    //let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
    //
    //let mut tls_settings =
    //    pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    //tls_settings.enable_h2();
    //lb.add_tls_with_settings("0.0.0.0:6189", None, tls_settings);

    my_server.add_service(lb_service);
    //my_server.add_service(upstreams_background);
    my_server.add_service(images_background);

    my_server.run_forever();
}
