use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use clap::Parser;
use http::{header, StatusCode};
use log::{error, info};
use moka::future::Cache;

use peserver::util::setup_logs;

// Note: this will double store the response for a gist at latest version if it is also requested
// at that specific version. Arc<Box<[u8]>> would allow sharing, but we don't know the latest
// version until we've already gotten it and we can't then change the key. Maybe a simpler cache
// with a map of RwLock would be better?

struct Ctx {
    client: pegh::Client,
    // can't use Arc<Box<[u8]>> because http_body::Body trait not implemented for it
    cache: Cache<String, Box<[u8]>>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    NotFound,
    Internal,
}

impl From<Error> for StatusCode {
    fn from(e: Error) -> StatusCode {
        match e {
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

async fn get_gist(
    State(ctx): State<Arc<Ctx>>,
    Path(gist): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    get_gist_impl(ctx, gist, None).await
}

async fn get_gist_version(
    State(ctx): State<Arc<Ctx>>,
    Path((gist, version)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    get_gist_impl(ctx, gist, Some(version)).await
}

async fn get_gist_impl(
    ctx: Arc<Ctx>,
    gist: String,
    version: Option<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let key = format!("{gist}:{}", version.as_deref().unwrap_or_default());
    let entry = ctx
        .cache
        .entry_by_ref(&key)
        .or_try_insert_with(retreive_gist(&ctx.client, &gist, version.as_deref()))
        .await
        .map_err(|e| StatusCode::from((*e).clone()))?;

    if entry.is_fresh() {
        info!("get_gist miss {key}");
    } else {
        info!("get_gist hit {key}");
    }
    let value: Box<[u8]> = entry.into_value();
    let cache_header = if version.is_some() {
        "immutable"
    } else {
        "max-age=3600"
    };
    let headers = [
        (header::CONTENT_TYPE, "application/json"),
        (header::CACHE_CONTROL, cache_header),
    ];
    Ok((headers, value))
}

async fn retreive_gist(
    client: &pegh::Client,
    gist: &str,
    version: Option<&str>,
) -> Result<Box<[u8]>, Error> {
    if let Some(gist) = client
        .get_gist(gist, version)
        .await
        .inspect_err(|e| error!("retreive_gist {gist}:{version:?} failed {e:?}"))
        .map_err(|_| Error::Internal)?
    {
        let bytes = serde_json::to_vec(&gist).map_err(|_| Error::Internal)?;
        Ok(bytes.into())
    } else {
        Err(Error::NotFound)
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long)]
    tcp: Option<String>,

    #[arg(long)]
    uds: Option<String>,

    #[arg(long, default_value_t = 100_000_000)]
    capacity: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    setup_logs();

    let args = Args::parse();
    let client = pegh::Client::new().unwrap();
    let cache = Cache::builder()
        .max_capacity(args.capacity)
        .weigher(|k: &String, v: &Box<[u8]>| (k.len() + v.len()).try_into().unwrap_or(u32::MAX))
        .build();

    let ctx = Arc::new(Ctx {
        client: client,
        cache: cache,
    });
    let app = Router::new()
        .route("/gist/{gist}", get(get_gist))
        .route("/gist/{gist}/{version}", get(get_gist_version))
        .with_state(ctx);

    match (args.tcp, args.uds) {
        (Some(addr), None) => {
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    tokio::signal::ctrl_c().await.unwrap();
                })
                .await
                .unwrap();
        }
        (None, Some(addr)) => {
            let _ = std::fs::remove_file(&addr);
            let listener = tokio::net::UnixListener::bind(addr).unwrap();
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    tokio::signal::ctrl_c().await.unwrap();
                })
                .await
                .unwrap();
        }
        (Some(_), Some(_)) => panic!("cannot use --tcp and --uds"),
        (None, None) => panic!("muse use --tcp or --uds"),
    };
}
