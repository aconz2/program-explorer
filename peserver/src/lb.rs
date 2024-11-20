use std::sync::Arc;
use std::time::Duration;
use std::collections::BTreeMap;

use pingora::prelude::RequestHeader;
use pingora::services::background::{background_service,BackgroundService};
use pingora::server::configuration::Opt;
use pingora::server::Server;
use pingora::upstreams::peer::HttpPeer;
use pingora::Result;
use pingora::lb::{health_check, selection::RoundRobin, LoadBalancer};
use pingora::proxy::{ProxyHttp, Session};
use pingora::http::{ResponseHeader};

use async_trait::async_trait;
use env_logger;
use bytes::{Bytes,BytesMut};
use http::{Method,StatusCode};
use http::header;
use arc_swap::ArcSwap;
use log::{error,info};
use serde_json;

use peserver::api::v1 as apiv1;

const TLS_FALSE: bool = false;

async fn read_full_body(session: &mut pingora::protocols::http::v1::client::HttpSession) -> Result<Bytes, Box<pingora::Error>> {
    let mut acc = BytesMut::with_capacity(4096);
    loop {
        match session.read_body_ref().await? {
            Some(ref bytes) => {
                acc.extend_from_slice(&bytes);
            }
            None => {
                break;
            }
        }
    }
    Ok(acc.freeze())
}

pub struct ImageData {
    image_map: BTreeMap<String, HttpPeer>,
    images: Vec<apiv1::images::Image>,
    premade_json: Bytes,
}

impl ImageData {
    fn from_parts(images: Vec<apiv1::images::Image>,
                  premade_json: Bytes,
                  image_map: BTreeMap<String, HttpPeer>,
                  ) -> Self {
        Self { images, premade_json, image_map }
    }
    fn new() -> Self {
        Self::from_parts(
            vec![],
            Bytes::from(serde_json::to_vec(&Vec::<apiv1::images::Image>::new()).unwrap()),
            BTreeMap::new(),
        )
    }
}

// TODO support more than one backend
pub struct Images {
    upstreams: Arc<LoadBalancer<RoundRobin>>,
    data: ArcSwap<ImageData>,
    image_check_frequency: Duration,
}

impl Images {
    fn new(upstreams: Arc<LoadBalancer<RoundRobin>>) -> Self {
        Self {
            upstreams,
            data: ArcSwap::from_pointee(ImageData::new()),
            image_check_frequency: Duration::from_secs(60),
        }
    }

    fn update_data(&self, peer: HttpPeer, body: Bytes, resp: apiv1::images::Response) {
        let map: BTreeMap<_, _> = {
            resp.images
                .iter()
                .map(|img| (img.info.digest.clone(), peer.clone()))
                .collect()
        };
        self.data.store(ImageData::from_parts(resp.images, body, map).into());
    }

    async fn do_update(&self) -> Result<(), Box<pingora::Error>> {
        let upstream = self
            .upstreams
            .select(b"", 256) // hash doesn't matter
            .ok_or_else(|| pingora::Error::new(pingora::ErrorType::ConnectProxyFailure))?;
        // TODO where are we going to get the certkey from for this?
        let peer = HttpPeer::new(upstream, TLS_FALSE, "".to_string());

        let connector = pingora::connectors::http::v1::Connector::new(None);
        let (mut session, _) = connector.get_http_session(&peer).await?;
        session.read_timeout = Some(Duration::from_secs(5));
        session.write_timeout = Some(Duration::from_secs(5));

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

        self.update_data(peer.clone(), body, resp);

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

            interval.tick().await;

            match self.do_update().await {
                Ok(()) => {}
                Err(e) => { error!("error getting images {:?}", e); }
            }
        }
    }
}

pub struct LB {
    upstreams: Arc<LoadBalancer<RoundRobin>>,
    images: Arc<Images>,
}

impl LB {
    fn new(upstreams: Arc<LoadBalancer<RoundRobin>>,  images: Arc<Images>) -> Self {
        Self { upstreams, images }
    }

    async fn apiv1_images(&self, session: &mut Session, _ctx: &mut <LB as ProxyHttp>::CTX) -> Result<()> {
        let downstream_session = &mut session.downstream_session;

        let buf = self.images.data.load().premade_json.clone();

        let response_header = {
            let mut x = ResponseHeader::build(200, Some(2)).unwrap();
            x.insert_header(header::CONTENT_TYPE, "application/json")?;
            x.insert_header(header::CONTENT_LENGTH, buf.len())?;
            Box::new(x)
        };

        downstream_session.write_response_header(response_header).await?;
        downstream_session.write_response_body(buf, true).await?;
        Ok(())
    }

    // Ok(true) means request done, ie the image was missed
    async fn apiv1_runi(&self, session: &mut Session, ctx: &mut <LB as ProxyHttp>::CTX) -> Result<bool> {
        let req_parts: &http::request::Parts = session.downstream_session.req_header();
        let uri_path_image = apiv1::runi::parse_path(req_parts.uri.path());

        let image_map = &self.images.data.load().image_map;

        match uri_path_image.and_then(|id| image_map.get(id)) {
            Some(p) => { *ctx = Some(p.clone()); Ok(false) }
            None => {
                // TODO use lazy static for 404
                let response_header = {
                    let mut x = ResponseHeader::build(404, Some(0)).unwrap();
                    x.insert_header(header::CONTENT_LENGTH, 0)?;
                    Box::new(x)
                };
                session.downstream_session
                    .write_response_header(response_header)
                    .await
                    .map(|_| true)
            }
        }
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
    // TODO maybe we can track connections per server with a hacky Drop on CTX
    type CTX = Option<HttpPeer>;

    fn new_ctx(&self) -> Self::CTX { None }

    // Ok(true) means request is done
    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let req_parts: &http::request::Parts = session.downstream_session.req_header();

        match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::GET,  apiv1::images::PATH) => self.apiv1_images(session, _ctx).await.map(|_| true),
            (Method::POST, path) if path.starts_with(apiv1::runi::PREFIX) => self.apiv1_runi(session, _ctx).await,
            _ => {
                // TODO use lazy static for 404
                let response_header = {
                    let mut x = ResponseHeader::build(404, Some(0)).unwrap();
                    x.insert_header(header::CONTENT_LENGTH, 0)?;
                    Box::new(x)
                };
                session.downstream_session
                    .write_response_header(response_header)
                    .await
                    .map(|_| true)
            }
        }
    }

    async fn proxy_upstream_filter(&self, _session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        Ok(ctx.is_some())
    }

    // TODO support multiple backends
    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        //let upstream = self
        //    .upstreams
        //    .select(b"", 256) // hash doesn't matter
        //    .ok_or_else(|| pingora::Error::new(pingora::ErrorType::ConnectProxyFailure))?;
        //
        //println!("upstream peer is: {:?}", upstream);
        //let peer = Box::new(HttpPeer::new(upstream, TLS_FALSE, "".to_string()));
        //Ok(peer)

        // we should always have Some here because we end requests early in proxy_upstream_filter
        ctx.take()
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

    let mut upstreams =
        LoadBalancer::try_from_iter(["127.0.0.1:1234"]).unwrap();

    assert!(upstreams.backends().get_backend().len() == 1, "only one backend supported right now");

    upstreams.set_health_check(health_check::TcpHealthCheck::new());
    upstreams.health_check_frequency = Some(Duration::from_secs(10));

    let lb_background = background_service("health check", upstreams);

    let upstreams = lb_background.task();

    let images = Images::new(upstreams.clone());
    let images_background = background_service("images", images);
    let images = images_background.task();

    let lb = LB::new(upstreams, images);
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
    my_server.add_service(lb_background);
    my_server.add_service(images_background);

    my_server.run_forever();
}
