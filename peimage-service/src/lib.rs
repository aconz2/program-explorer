use std::io::IoSliceMut;
use std::os::fd::OwnedFd;
use std::path::Path;

use oci_spec::{
    distribution::Reference,
    image::{Arch, Digest, Os},
};
use tokio_seqpacket::{UnixSeqpacket, ancillary::OwnedAncillaryMessage};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    Io(#[from] std::io::Error),
    Encode(#[from] bincode::error::EncodeError),
    Decode(#[from] bincode::error::DecodeError),
    BadDigest,
    MissingFd,
    ServerError(String),
    Unknown,

    // these are the user facing errors
    NoMatchingManifest,
    ManifestNotFound,
    ImageTooBig,
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
pub struct Request {
    reference: String,
    //arch: Arch,
    //os: Os,
    // TODO I think this has to take a duration since we'd rather not have the requester do a
    // timeout and cancel the request
}

impl Request {
    pub fn new(reference: &Reference, _arch: Arch, _os: Os) -> Self {
        Request {
            reference: reference.to_string(),
            //arch,
            //os,
        }
    }
}

impl Request {
    pub fn parse_reference(&self) -> Option<Reference> {
        self.reference.parse().ok()
    }
}

// this should maybe not be pub but pub(crate) doesn't work with main.rs I think?
#[derive(Debug, bincode::Encode, bincode::Decode)]
pub enum WireResponse {
    Ok { manifest_digest: String },
    NoMatchingManifest,
    ManifestNotFound,
    ImageTooBig,
    Err { message: String },
}

pub struct Response {
    pub manifest_digest: Digest,
    pub fd: OwnedFd,
}

pub async fn request_erofs_image(
    socket_addr: impl AsRef<Path>,
    req: Request,
) -> Result<Response, Error> {
    let socket = UnixSeqpacket::connect(socket_addr).await?;
    let mut buf = [0; 1024];
    let n = bincode::encode_into_slice(&req, &mut buf, bincode::config::standard())?;
    let _ = socket.send(&buf[..n]).await?;

    let mut ancillary_buffer = [0; 128];
    let (n, ancillary) = socket
        .recv_vectored_with_ancillary(&mut [IoSliceMut::new(&mut buf)], &mut ancillary_buffer)
        .await?;

    let (wire_response, _) =
        bincode::decode_from_slice::<WireResponse, _>(&buf[..n], bincode::config::standard())?;

    let fd = if let Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) =
        ancillary.into_messages().next()
    {
        fds.next()
    } else {
        None
    };

    match (fd, wire_response) {
        (Some(fd), WireResponse::Ok { manifest_digest }) => Ok(Response {
            manifest_digest: manifest_digest.parse().map_err(|_| Error::BadDigest)?,
            fd,
        }),
        (_, WireResponse::NoMatchingManifest) => Err(Error::NoMatchingManifest),
        (_, WireResponse::ManifestNotFound) => Err(Error::ManifestNotFound),
        (_, WireResponse::ImageTooBig) => Err(Error::ImageTooBig),
        (_, WireResponse::Err { message }) => Err(Error::ServerError(message)),
        (None, _) => Err(Error::MissingFd),
    }
}
