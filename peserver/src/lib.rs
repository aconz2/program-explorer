pub mod api;
pub mod util;
pub mod admin;

use pingora::http::ResponseHeader;
use bytes::Bytes;

pub struct StaticFile {
    etag: Option<String>,
    uncompressed: (ResponseHeader, Bytes),
    compressed: Option<(ResponseHeader, Bytes)>, // gzip for now
}

impl StaticFile {
    fn for_request(&self, parts: &http::request::Parts) -> (ResponseHeader, Bytes) {
        // todo check etag and respond 304 if okay
        let accept_encoding_bytes = parts.headers.get(http::header::ACCEPT_ENCODING).map(|x| x.as_bytes());
        match (&self.compressed, accept_encoding_bytes) {
            (Some(ref comp), Some(b"gzip"))                      => comp.clone(),
            (Some(ref comp), Some(s)) if s.starts_with(b"gzip,") => comp.clone(),
            _ => self.uncompressed.clone(),
        }
    }
}

