use std::time::Duration;

pub const APPLICATION_JSON: &str = "application/json";
pub const APPLICATION_X_PE_ARCHIVEV1: &str = "application/x.pe.archivev1";

// max request per second per client
pub const MAX_REQ_PER_SEC: isize = 2;
// max time we will wait trying to get a place in line for the worker
// browsers are maybe a 60s total timeout so we have to get in there pretty quick to then hope to
// actually get our request through
pub const MAX_BODY_SIZE: usize = 65536;
pub const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
// these are per read/write call
pub const DOWNSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub const DOWNSTREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub enum ContentType {
    ApplicationJson,
    PeArchiveV1, // <u32 json size> <json> <pearchivev1>
}

impl TryFrom<&str> for ContentType {
    type Error = ();

    fn try_from(s: &str) -> Result<ContentType, ()> {
        match s {
            APPLICATION_JSON => Ok(ContentType::ApplicationJson),
            APPLICATION_X_PE_ARCHIVEV1 => Ok(ContentType::PeArchiveV1),
            _ => Err(()),
        }
    }
}

impl From<ContentType> for &str {
    fn from(val: ContentType) -> Self {
        match val {
            ContentType::ApplicationJson => APPLICATION_JSON,
            ContentType::PeArchiveV1 => APPLICATION_X_PE_ARCHIVEV1,
        }
    }
}

pub mod v2 {
    pub mod runi {
        use super::super::ContentType;
        use peinit;
        use serde::{Deserialize, Serialize};
        use oci_spec::image::{Arch, Os};

        pub const PREFIX: &str = "/api/v2/runi/";

        #[derive(Serialize, Deserialize)]
        pub struct Request {
            pub stdin: Option<String>, // filename that will be set as stdin, noop
            // for content-type: application/json
            pub entrypoint: Option<Vec<String>>, // as per oci image config
            pub cmd: Option<Vec<String>>,        // as per oci image config
            pub env: Option<Vec<String>>,        // as per oci image config
        }

        pub type Response = peinit::Response;

        #[derive(Debug)]
        pub struct ParsedPath<'a> {
            pub reference: &'a str,
            pub arch: Arch,
            pub os: Os,
        }

        // TODO would be nice to validate the reference I think? Right now we push string all the
        // way through to image-service so that it is a single string but could probably add a
        // peoci_spec::Reference with each field registry, repository, and TagOrDigest
        // /api/v2/runi/<arch>/<os>/<reference>
        pub fn parse_path<'a>(s: &'a str) -> Option<ParsedPath<'a>> {
            let rest = s.strip_prefix(PREFIX)?;
            let (arch, rest) = rest.split_once('/')?;
            let (os, reference) = rest.split_once('/')?;
            // https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pulling-manifests
            if reference.len() > 255 {
                return None;
            }
            Some(ParsedPath {
                reference,
                arch: arch.try_into().ok()?,
                os: os.try_into().ok()?,
            })
        }

        pub fn parse_request(body: &[u8], content_type: &ContentType) -> Option<(usize, Request)> {
            match content_type {
                ContentType::ApplicationJson => {
                    let req = serde_json::from_slice(body).ok()?;
                    Some((0, req))
                }
                ContentType::PeArchiveV1 => {
                    if body.len() < 4 {
                        return None;
                    }
                    let json_size =
                        u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
                    let slice = body.get(4..4 + json_size)?;
                    let req = serde_json::from_slice(slice).ok()?;
                    Some((4 + json_size, req))
                }
            }
        }

        // assumes pearchivev1 format
        // <u32: response size> <response json> <archive>
        pub fn parse_response(body: &[u8]) -> Option<(Response, &[u8])> {
            if body.len() < 4 {
                return None;
            }
            let json_size = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
            let slice = body.get(4..4 + json_size)?;
            let response: Response = serde_json::from_slice(slice).ok()?;
            let rem = body.get(4 + json_size..)?;
            Some((response, rem))
        }
    }
}
