use std::collections::BTreeMap;
use std::sync::Arc;

use log::{error, trace};
use reqwest::{Method, Response, StatusCode, header};
use serde::Serialize;
use tokio::{sync::Semaphore, task::JoinSet};

// user agent required
// the "Fine-grained personal access tokens" are not that fine-grained and you can only grant
// read+write to gists, so I'd rather just stick to unauthenticated for now?
const USER_AGENT: &str = "aconz2";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    Reqwest(#[from] reqwest::Error),
    StatusNotOk(StatusCode),
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
    sem: Arc<Semaphore>,
}

impl Client {
    pub fn new() -> Result<Self, Error> {
        let client = reqwest::Client::builder().https_only(true).build()?;
        Ok(Self {
            client,
            sem: Arc::new(Semaphore::new(8)),
        })
    }

    pub async fn get_gist(&self, id: &str) -> Result<Option<Gist>, Error> {
        self.get_gist_impl(id, None).await
    }

    pub async fn get_gist_version(&self, id: &str, revision: &str) -> Result<Option<Gist>, Error> {
        self.get_gist_impl(id, Some(revision)).await
    }

    // https://docs.github.com/en/rest/gists/gists?apiVersion=2022-11-28#get-a-gist
    // https://docs.github.com/en/rest/gists/gists?apiVersion=2022-11-28#get-a-gist-revision
    async fn get_gist_impl(&self, id: &str, revision: Option<&str>) -> Result<Option<Gist>, Error> {
        let _guard = self.sem.acquire().await;
        let url = format!(
            "https://api.github.com/gists/{}{}{}",
            id,
            if revision.is_some() { "/" } else { "" },
            revision.unwrap_or_default()
        );
        let res = self
            .client
            .request(Method::GET, &url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?;

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
                let mut set = JoinSet::new();
                for (name, file) in gist.files {
                    if file.truncated {
                        trace!("file is truncated");
                        let client = self.client.clone();
                        let sem = self.sem.clone();
                        set.spawn(async move {
                            (name, Client::get_raw_url(client, sem, &file.raw_url).await)
                        });
                    } else {
                        files.insert(name, file.content);
                    }
                }
                while let Some(next) = set.join_next().await {
                    match next {
                        Ok((name, Ok(content))) => {
                            files.insert(name, content);
                        }
                        Ok((_, Err(e))) => {
                            return Err(e);
                        }
                        Err(e) => {
                            error!("unknown error {:?}", e);
                            return Err(Error::Unknown);
                        }
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

    // This is a bit of a hack since we do this in a JoinSet and can't move self
    // What is the right way to deal with this? Do I need a wrapper with a single
    // Arc<InnerClient> thing?
    async fn get_raw_url(
        client: reqwest::Client,
        sem: Arc<Semaphore>,
        url: &str,
    ) -> Result<String, Error> {
        let _guard = sem.acquire().await;
        let res = client
            .request(Method::GET, url)
            .header(header::USER_AGENT, USER_AGENT)
            .send()
            .await?;
        match res.status() {
            StatusCode::OK => Ok(res.text().await?),
            _ => Err(status_not_ok(res).await),
        }
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
