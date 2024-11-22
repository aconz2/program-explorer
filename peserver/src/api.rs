use std::time::Duration;

use pingora::http::ResponseHeader;
use http::header;

pub const APPLICATION_JSON: &str = "application/json";
pub const APPLICATION_X_PE_ARCHIVEV1: &str = "application/x.pe.archivev1";

// max request per second per client
pub const MAX_REQ_PER_SEC: isize = 1;
// max time we will wait trying to get a place in line for the worker
// browsers are maybe a 60s total timeout so we have to get in there pretty quick to then hope to
// actually get our request through
pub const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

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

impl Into<&str> for ContentType {
    fn into(self) -> &'static str {
        match self {
            ContentType::ApplicationJson => APPLICATION_JSON,
            ContentType::PeArchiveV1 => APPLICATION_X_PE_ARCHIVEV1,
        }
    }
}

pub mod v1 {
    pub mod runi {
        use super::super::ContentType;
        use serde::{Deserialize};
        use peinit;

        pub const PREFIX: &str = "/api/v1/runi/";

        #[derive(Deserialize)]
        pub struct Request {
            pub stdin      : Option<String>,       // filename that will be set as stdin, noop
                                                   // for content-type: application/json
            pub entrypoint : Option<Vec<String>>,  // as per oci image config
            pub cmd        : Option<Vec<String>>,  // as per oci image config
        }

        pub type Response = peinit::Response;

        // /api/v1/runi/<algo>:<digest>
        //              [-------------]
        // doesn't fully check things, but covers the basics
        pub fn parse_path(s: &str) -> Option<&str> {
            let x = s.strip_prefix(PREFIX)?;
            if x.len() > 135 { return None; }  // this is length of sha512:...
            if x.contains("/") { return None; }
            Some(x)
        }

        pub fn parse_request(body: &[u8], content_type: &ContentType) -> Option<Request> {
            match content_type {
                ContentType::ApplicationJson => {
                    serde_json::from_slice(&body).ok()
                }
                ContentType::PeArchiveV1 => {
                    if body.len() < 4 { return None; }
                    let json_size = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
                    serde_json::from_slice(&body[4..4+json_size]).ok()
                }
            }
        }

    }

    pub mod images {
        use serde::{Deserialize,Serialize};
        use peimage;
        use oci_spec::image as oci_image;
        use super::runi;

        pub const PATH: &str = "/api/v1/images";

        #[derive(Deserialize,Serialize)]
        pub struct ImageLinks {
            pub runi: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            pub upstream: Option<String>,
        }

        #[derive(Deserialize,Serialize)]
        pub struct Image {
            pub links: ImageLinks,
            pub info: peimage::PEImageId,
            pub config: oci_image::ImageConfiguration,
        }

        #[derive(Deserialize,Serialize)]
        pub struct Response {
            pub images: Vec<Image>,
        }

        impl From<&peimage::PEImageMultiIndex> for Response {
            fn from(index: &peimage::PEImageMultiIndex) -> Self {
                let images: Vec<_> = index.map().iter()
                    .map(|(_k, v)| {
                        Image {
                            links: ImageLinks {
                                runi: format!("{}/{}", runi::PREFIX, v.image.id.digest),
                                upstream: v.image.id.upstream_link(),
                            },
                            info: v.image.id.clone(),
                            config: v.image.config.clone(),
                        }
                    })
                    .collect();
                Self { images }
            }
        }
    }
}

pub fn make_json_response_header(len: usize) -> ResponseHeader {
    let mut x = ResponseHeader::build(200, Some(2)).unwrap();
    x.insert_header(header::CONTENT_TYPE, "application/json").unwrap();
    x.insert_header(header::CONTENT_LENGTH, len).unwrap();
    x
}

pub mod premade_errors {
    use once_cell::sync::Lazy;
    use pingora::protocols::http::error_resp;
    use pingora::http::ResponseHeader;
    use http::StatusCode;
    use super::MAX_REQ_PER_SEC;

    // annoyingly this doesn't work because status gets captured
    //fn e(status: StatusCode) -> Lazy<ResponseHeader> {
    //    Lazy::new(move || error_resp::gen_error_response(status.into()))
    //}

    pub static NOT_FOUND: Lazy<ResponseHeader> = Lazy::new(|| error_resp::gen_error_response(StatusCode::NOT_FOUND.into()));
    pub static INTERNAL_SERVER_ERROR: Lazy<ResponseHeader> = Lazy::new(|| error_resp::gen_error_response(StatusCode::INTERNAL_SERVER_ERROR.into()));
    pub static SERVICE_UNAVAILABLE: Lazy<ResponseHeader> = Lazy::new(|| error_resp::gen_error_response(StatusCode::SERVICE_UNAVAILABLE.into()));

    pub static TOO_MANY_REQUESTS: Lazy<ResponseHeader> = Lazy::new(|| {
            let mut header = ResponseHeader::build(StatusCode::TOO_MANY_REQUESTS, Some(3)).unwrap();
            header
                .insert_header("X-Rate-Limit-Limit", MAX_REQ_PER_SEC.to_string())
                .unwrap();
            header.insert_header("X-Rate-Limit-Remaining", "0").unwrap();
            header.insert_header("X-Rate-Limit-Reset", "1").unwrap();
            header.insert_header("Content-Length", "0").unwrap();
            header
    });
}

