use std::collections::HashMap;

use pingora::http::ResponseHeader;
use bytes::Bytes;
use bincode;
use http::StatusCode;

use serde::{Serialize,Deserialize};

pub struct StaticFile {
    etag: Option<String>,
    uncompressed: (ResponseHeader, Bytes),
    compressed: Option<(ResponseHeader, Bytes)>, // gzip for now
}

impl StaticFile {
    pub fn for_request(&self, parts: &http::request::Parts) -> (ResponseHeader, Bytes) {
        // todo check etag and respond 304 if okay
        let accept_encoding_bytes = parts.headers.get(http::header::ACCEPT_ENCODING).map(|x| x.as_bytes());
        match (&self.compressed, accept_encoding_bytes) {
            (Some(ref comp), Some(b"gzip"))                      => comp.clone(),
            (Some(ref comp), Some(s)) if s.starts_with(b"gzip,") => comp.clone(),
            _ => self.uncompressed.clone(),
        }
    }
}

pub enum Error {
    Deser,
    BadPath,
    Duplicate,
    Header,
}

#[derive(Serialize,Deserialize)]
pub struct StaticFileEntry {
    etag: Option<String>,
    path: String,
    headers: Vec<(String, String)>,
    data: Vec<u8>,
    //gzip_compressed: Option<Vec<u8>>,
}

impl TryInto<StaticFile> for StaticFileEntry {
    type Error = Error;
    fn try_into(self) -> Result<StaticFile, Error> {
        let mut res = ResponseHeader::build(StatusCode::OK, Some(self.headers.len())).unwrap();
        for (k, v) in self.headers {
            res.insert_header(k, v).map_err(|_| Error::Header)?;
        }
        Ok(StaticFile {
            etag: self.etag,
            uncompressed: (res, self.data.into()),
            compressed: None,
        })
    }
}

type StaticFileBundle = Vec<StaticFileEntry>;

pub fn static_file_map_from_buf(data: &[u8]) -> Result<HashMap<String, StaticFile>, Error> {
    let entries: StaticFileBundle = bincode::deserialize(&data).map_err(|_| Error::Deser)?;
    let mut acc = HashMap::new();
    for entry in entries {
        if acc.contains_key(&entry.path) { return Err(Error::Duplicate); }
        let path = entry.path.clone();
        acc.insert(path, entry.try_into()?);
    }
    Ok(acc)
}

#[derive(Debug)]
pub enum BundleBuilderError {
    Duplicate,
}

//#[cfg(test)]
//mod tests {
//    use super::*;
//
//    #[test]
//    fn make_bundle() {
//        let mut builder = StaticFileBundleBuilder::new();
//        builder.add_entry("/", b"<html></html>").unwrap();
//        let buf = builder.into_vec();
//        println!("{:?}", buf);
//    }
//}
