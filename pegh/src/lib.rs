use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::{StreamExt, stream::FuturesUnordered};
use log::{error, trace, warn};
use reqwest::{Method, Response, StatusCode, header, header::HeaderValue};
use serde::Serialize;
use tokio::sync::{RwLock, Semaphore};

// I think gist truncation happens around 1 MB. this gist has 1 non-truncated and 2 truncated files
// for testing: https://gist.github.com/aconz2/a7359c6e3a5704af841389b85dda1e49

// user agent required
// the "Fine-grained personal access tokens" are not that fine-grained and you can only grant
// read+write to gists, so I'd rather just stick to unauthenticated for now?
const USER_AGENT: &str = "aconz2";

// if they don't send ratelimit-reset, default to 1 minute (guessing)
const DEFAULT_RATELIMIT_RESET: u64 = 60;

type UtcInstant = DateTime<Utc>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    Reqwest(#[from] reqwest::Error),
    StatusNotOk(StatusCode),
    RatelimitExceeded,
    NoHistory,
    Unknown,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Serialize)]
pub struct Gist {
    pub files: BTreeMap<String, String>,
    pub version: String,
    pub versions: Vec<String>,
}

mod wire {
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    pub(crate) struct File {
        pub(crate) raw_url: String,
        pub(crate) truncated: bool,
        pub(crate) content: String,
    }

    #[derive(Deserialize)]
    pub(crate) struct History {
        pub(crate) version: String,
    }

    #[derive(Deserialize)]
    pub(crate) struct Gist {
        pub(crate) files: BTreeMap<String, File>,
        pub(crate) history: Vec<History>,
    }
}

pub struct Client {
    client: reqwest::Client,
    sem: Semaphore,
    ratelimit: RwLock<Option<UtcInstant>>,
}

impl Client {
    pub fn new() -> Result<Self, Error> {
        let client = reqwest::Client::builder().https_only(true).build()?;
        Ok(Self {
            client,
            // https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api?apiVersion=2022-11-28#avoid-concurrent-requests
            sem: Semaphore::new(1),
            ratelimit: RwLock::new(None),
        })
    }

    pub async fn get_gist_latest(&self, id: &str) -> Result<Option<Gist>, Error> {
        self.get_gist(id, None).await
    }

    pub async fn get_gist_version(&self, id: &str, revision: &str) -> Result<Option<Gist>, Error> {
        self.get_gist(id, Some(revision)).await
    }

    // https://docs.github.com/en/rest/gists/gists?apiVersion=2022-11-28#get-a-gist
    // https://docs.github.com/en/rest/gists/gists?apiVersion=2022-11-28#get-a-gist-revision
    pub async fn get_gist(&self, id: &str, revision: Option<&str>) -> Result<Option<Gist>, Error> {
        self.check_ratelimit().await?;


        let url = format!(
            "https://api.github.com/gists/{}{}{}",
            id,
            if revision.is_some() { "/" } else { "" },
            revision.unwrap_or_default()
        );

        let res = {
            let _guard = self.sem.acquire().await;

            self
                .client
                .request(Method::GET, &url)
                .header(header::USER_AGENT, USER_AGENT)
                .header(header::ACCEPT, "application/vnd.github+json")
                .send()
                .await?
        };

        self.handle_ratelimit(&res).await?;

        if log::log_enabled!(log::Level::Trace) {
            for (header, value) in res.headers().iter() {
                trace!("header {}: {:?}", header, value);
            }
        }

        match res.status() {
            StatusCode::OK => {
                let gist = res.json::<wire::Gist>().await?;
                let version = if let Some(v) = revision {
                    v.to_string()
                } else {
                    let h = gist.history.last().ok_or(Error::NoHistory)?;
                    h.version.clone()
                };
                let versions = gist.history.into_iter().map(|h| h.version).collect();
                let mut files = BTreeMap::new();
                let mut futs = FuturesUnordered::new();
                for (name, file) in gist.files {
                    if file.truncated {
                        trace!("file is truncated");
                        let url = file.raw_url.to_string();
                        futs.push(async { (name, self.get_raw_url(url).await) });
                    } else {
                        files.insert(name, file.content);
                    }
                }

                while let Some((name, contents)) = futs.next().await {
                    match contents {
                        Ok(contents) => {
                            files.insert(name, contents);
                        }
                        Err(e) => return Err(e),
                    }
                }

                Ok(Some(Gist {
                    files,
                    version,
                    versions,
                }))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(status_not_ok(res).await),
        }
    }

    async fn get_raw_url(&self, url: String) -> Result<String, Error> {
        self.check_ratelimit().await?;

        let _guard = self.sem.acquire().await;

        let res = self
            .client
            .request(Method::GET, url)
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await?;

        self.handle_ratelimit(&res).await?;

        match res.status() {
            StatusCode::OK => Ok(res.text().await?),
            _ => Err(status_not_ok(res).await),
        }
    }

    async fn handle_ratelimit(&self, res: &Response) -> Result<(), Error> {
        // ghcr apparently returns either 403 or 429
        if !matches!(
            res.status(),
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        ) {
            return Ok(());
        }

        let end = if let Some(reset) = get_ratelimit_reset_header(res.headers()) {
            reset
                .try_into() // u64 -> i64
                .ok()
                .and_then(|x| chrono::DateTime::<chrono::Utc>::from_timestamp(x, 0))
                .unwrap_or_else(|| {
                    error!("bad reset timestamp");
                    Utc::now() + Duration::from_secs(DEFAULT_RATELIMIT_RESET)
                })
        } else {
            warn!("got res status {} but no ratelimit-reset", res.status());
            Utc::now() + Duration::from_secs(DEFAULT_RATELIMIT_RESET)
        };
        let _ = self.ratelimit.write().await.insert(end);

        Err(Error::RatelimitExceeded)
    }

    async fn check_ratelimit(&self) -> Result<(), Error> {
        let mut remove = false;
        if let Some(ratelimit_end) = *self.ratelimit.read().await {
            if Utc::now() < ratelimit_end {
                warn!("still in ratelimit reset period");
                return Err(Error::RatelimitExceeded);
            } else {
                remove = true;
            }
        }
        if remove {
            let _ = self.ratelimit.write().await.take();
        }
        Ok(())
    }
}

async fn status_not_ok(res: Response) -> Error {
    let status = res.status();
    if log::log_enabled!(log::Level::Trace) {
        match res.text().await {
            Ok(body) => {
                trace!("status={}, body={}", status, body);
            }
            Err(e) => {
                trace!("unhandled error getting body, status={status}, error={e:?}");
            }
        }
    }
    Error::StatusNotOk(status)
}

// copied from ocidist.rs
// https://www.ietf.org/archive/id/draft-polli-ratelimit-headers-02.html#section-3.3
// returns whatever number is in the header. RFC says it is the number of seconds until reset, but
// github specify that it is the timestamp when it resets! github says "UTC epoch seconds"
// https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api?apiVersion=2022-11-28#checking-the-status-of-your-rate-limit
fn get_ratelimit_reset_header(map: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(value) = map.get("ratelimit-reset") {
        parse_ratelimit_reset_header(value)
    } else if let Some(value) = map.get("x-ratelimit-reset") {
        parse_ratelimit_reset_header(value)
    } else {
        None
    }
}

fn parse_ratelimit_reset_header(input: &HeaderValue) -> Option<u64> {
    parse_ratelimit_reset_str(input.to_str().ok()?)
}

fn parse_ratelimit_reset_str(input: &str) -> Option<u64> {
    input.parse().ok()
}
