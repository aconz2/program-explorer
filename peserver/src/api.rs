pub const APPLICATION_JSON: &str = "application/json";
pub const APPLICATION_X_PE_ARCHIVEV1: &str = "application/x.pe.archivev1";

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

        // /api/v1/runi/<algo>/<digest>
        //              [-------------]
        // doesn't fully check things, but covers the basics
        pub fn parse_path(s: &str) -> Option<&str> {
            //if !s.starts_with(ROUTE_API_V1_RUNI) { return None; }
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
