use std::collections::HashMap;
use std::io::Read;

use pingora::http::ResponseHeader;
use bytes::Bytes;
use bincode;
use http::StatusCode;

use serde::{Serialize,Deserialize};

pub struct StaticFile {
    pub etag: Option<String>,
    pub uncompressed: (ResponseHeader, Bytes),
    pub gzip: Option<(ResponseHeader, Bytes)>, // gzip for now
}

impl StaticFile {
    pub fn for_request(&self, parts: &http::request::Parts) -> (ResponseHeader, Bytes) {
        // todo check etag and respond 304 if okay
        let accept_encoding_bytes = parts.headers.get(http::header::ACCEPT_ENCODING).map(|x| x.as_bytes());
        match (&self.gzip, accept_encoding_bytes) {
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
    Io,
}

#[derive(Serialize,Deserialize)]
pub struct StaticFileEntry {
    pub etag: Option<String>,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub data: Vec<u8>,
    pub gzip: Option<Vec<u8>>,
}

impl TryInto<StaticFile> for StaticFileEntry {
    type Error = Error;
    fn try_into(self) -> Result<StaticFile, Error> {
        let mut res = ResponseHeader::build(StatusCode::OK, Some(self.headers.len() + 1)).unwrap();
        for (k, v) in self.headers {
            res.insert_header(k, v).map_err(|_| Error::Header)?;
        }
        let gzip = self.gzip.map(|data| {
            let mut res = res.clone();
            res.insert_header("Content-encoding", "gzip").map_err(|_| Error::Header)?;
            Ok((res, data.into()))
        }).transpose()?;
        Ok(StaticFile {
            etag: self.etag,
            uncompressed: (res, self.data.into()),
            gzip: gzip,
        })
    }
}

pub type StaticFileBundle = Vec<StaticFileEntry>;

pub fn static_file_map_from_reader<R: Read>(reader: &mut R) -> Result<HashMap<String, StaticFile>, Error> {
    let entries: StaticFileBundle = bincode::deserialize_from(reader).map_err(|_| Error::Deser)?;
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
