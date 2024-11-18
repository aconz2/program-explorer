use async_trait::async_trait;
//use clap::Parser;
use std::sync::Arc;
use std::time::Duration;

use env_logger;

use pingora::services::background::background_service;
use pingora::server::configuration::Opt;
use pingora::server::Server;
use pingora::upstreams::peer::HttpPeer;
use pingora::Result;
use pingora::lb::{health_check, selection::RoundRobin, LoadBalancer};
use pingora::proxy::{ProxyHttp, Session};

pub struct LB(Arc<LoadBalancer<RoundRobin>>);

const TLS_FALSE: bool = false;

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    // so if we do peer selection by reading the body, I think that is maybe not great
    // b/c we have to deal with the two request formats etc.
    // maybe we should put the data into /api/v1/runi/linux/amd64?image=index.docker.io/library/busybox:1.36.1
    // now just do /api/v1/runi/sha256/kj
    // the colon doesn't work in the url so maybe it has to go as a query param, at least doesn't
    // have to get escaped there
    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        let upstream = self
            .0
            .select(b"", 256) // hash doesn't matter
            .unwrap();

        println!("upstream peer is: {:?}", upstream);

        let peer = Box::new(HttpPeer::new(upstream, TLS_FALSE, "one.one.one.one".to_string()));
        Ok(peer)
    }

    async fn proxy_upstream_filter(&self, _session: &mut Session, _ctx: &mut ()) -> Result<bool> {
        Ok(true)
    }

    //async fn upstream_request_filter(
    //    &self,
    //    _session: &mut Session,
    //    upstream_request: &mut pingora::http::RequestHeader,
    //    _ctx: &mut Self::CTX,
    //) -> Result<()> {
    //    upstream_request
    //        .insert_header("Host", "one.one.one.one")
    //        .unwrap();
    //    Ok(())
    //}
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

    // We add health check in the background so that the bad server is never selected.
    let hc = health_check::TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    upstreams.health_check_frequency = Some(Duration::from_secs(10));

    let background = background_service("health check", upstreams);

    let upstreams = background.task();

    let mut lb = pingora::proxy::http_proxy_service(&my_server.configuration, LB(upstreams));
    lb.add_tcp("127.0.0.1:6188");

    //let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    //let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
    //
    //let mut tls_settings =
    //    pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    //tls_settings.enable_h2();
    //lb.add_tls_with_settings("0.0.0.0:6189", None, tls_settings);

    my_server.add_service(lb);
    my_server.add_service(background);
    my_server.run_forever();
}
