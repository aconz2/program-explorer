use std::collections::HashMap;
use std::sync::Arc;
use std::path::PathBuf;
use std::fs::File;

use http::{Method,Response,StatusCode};
use arc_swap::ArcSwap;
use serde::Serialize;
use async_trait::async_trait;

use pingora::protocols::http::ServerSession;
use pingora::apps::http_app::ServeHttp;

use crate::staticfiles::{StaticFile,static_file_map_from_reader};
use crate::util::{response_json,response_no_body};

#[derive(Debug,Serialize,Clone)]
pub enum Error {
    ReadError,
    BadBody,
    Serde,
    Io,
}

pub struct Admin {
    files_source: PathBuf,
    static_files: Arc<ArcSwap<HashMap<String, StaticFile>>>,
}

impl Admin {
    pub fn new<P: Into<PathBuf>>(files_source: P, static_files: Arc<ArcSwap<HashMap<String, StaticFile>>>) -> Result<Self, Error> {
        let files_source = files_source.into();
        let initial_data = static_file_map_from_reader(&mut File::open(&files_source).map_err(|_| Error::Io)?)
            .map_err(|_| Error::Serde)?;
        static_files.store(initial_data.into());
        Ok(Self { files_source, static_files })
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: Error,
}

#[derive(Serialize)]
struct StaticFileResponseEntry {
    path: String,
    etag: Option<String>,
    headers: Vec<(String, String)>,
}

#[derive(Serialize)]
struct StaticFileResponse {
    files: Vec<StaticFileResponseEntry>,
}

impl Into<Response<Vec<u8>>> for Error {
    fn into(self: Error) -> Response<Vec<u8>> {
        // response_no_body(self.into())
        response_json(StatusCode::BAD_REQUEST, ErrorBody{ error: self }).unwrap()
    }
}

impl Admin {
    async fn update_static_files(&self, _session: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        let data = static_file_map_from_reader(&mut File::open(&self.files_source).map_err(|_| Error::Io)?)
            .map_err(|_| Error::Serde)?;
        self.static_files.store(data.into());
        Ok(response_no_body(StatusCode::OK))
    }

    async fn get_static_files(&self, _session: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        let entries = self.static_files.load()
            .iter()
            .map(|(path, static_file)| StaticFileResponseEntry {
                path: path.clone(),
                etag: static_file.etag.clone(),
                headers: static_file.uncompressed.0.as_owned_parts()
                    .headers.iter()
                    .map(|(k, v)| (k.as_str().into(), v.to_str().unwrap_or("ERROR").into()))
                    .collect(),
            })
            .collect();
        response_json(StatusCode::OK, StaticFileResponse{files: entries})
            .map_err(|_| Error::Serde)
    }
}

#[async_trait]
impl ServeHttp for Admin {
    async fn response(&self, session: &mut ServerSession) -> Response<Vec<u8>> {
        let req_parts: &http::request::Parts = session.req_header();
        let res = match (req_parts.method.clone(), req_parts.uri.path()) {
            (Method::GET,  "/static") => self.get_static_files(session).await,
            (Method::POST, "/static") => self.update_static_files(session).await,
            _ => {
                return response_no_body(StatusCode::NOT_FOUND)
            }
        };
        res.unwrap_or_else(|e| e.into())
    }
}

