use std::collections::HashMap;
use std::sync::Arc;

use http::{Method,Response,StatusCode};
use bytes::Bytes;
use arc_swap::ArcSwap;
use serde::Serialize;
use async_trait::async_trait;

use pingora::protocols::http::ServerSession;
use pingora::apps::http_app::ServeHttp;

use crate::staticfiles::{StaticFile,static_file_map_from_buf};
use crate::util::{
    read_full_server_request_body,
    response_json,response_no_body
};

pub struct Admin {
    static_files: Arc<ArcSwap<HashMap<String, StaticFile>>>,
}

impl Admin {
    pub fn new(static_files: Arc<ArcSwap<HashMap<String, StaticFile>>>) -> Self {
        Self {
            static_files,
        }
    }
}

#[derive(Debug,Serialize,Clone)]
enum Error {
    ReadError,
    BadBody,
    Serde,
}

#[derive(Serialize)]
struct ErrorBody {
    error: Error,
}


#[derive(Serialize)]
struct StaticFileResponse {
}

impl Into<Response<Vec<u8>>> for Error {
    fn into(self: Error) -> Response<Vec<u8>> {
        // response_no_body(self.into())
        response_json(StatusCode::BAD_REQUEST, ErrorBody{ error: self }).unwrap()
    }
}

impl Admin {
    async fn update_static_files(&self, session: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        let body = read_full_server_request_body(session, 2_000_000).await
            .map_err(|_| Error::ReadError)?;
        let static_files = static_file_map_from_buf(&body)
            .map_err(|_| Error::BadBody)?;
        self.static_files.store(static_files.into());
        Ok(response_no_body(StatusCode::OK))
    }

    async fn get_static_files(&self, _session: &mut ServerSession) -> Result<Response<Vec<u8>>, Error> {
        response_json(StatusCode::OK, StaticFileResponse{})
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

