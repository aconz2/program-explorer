use std::io::IoSliceMut;
use std::os::fd::OwnedFd;
use std::path::Path;

use oci_spec::distribution::Reference;
use tokio_seqpacket::{UnixSeqpacket, ancillary::OwnedAncillaryMessage};

#[derive(Debug)]
pub enum Error {
    Io,
    Unknown,
    Encode,
    Decode,
}

impl From<std::io::Error> for Error {
    fn from(_e: std::io::Error) -> Self {
        Error::Io
    }
}

impl From<bincode::error::EncodeError> for Error {
    fn from(_e: bincode::error::EncodeError) -> Self {
        Error::Encode
    }
}

impl From<bincode::error::DecodeError> for Error {
    fn from(_e: bincode::error::DecodeError) -> Self {
        Error::Decode
    }
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
pub struct Request {
    reference: String,
}

impl Request {
    pub fn parse_reference(&self) -> Option<Reference> {
        self.reference.parse().ok()
    }
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
struct WireResponse {}

pub struct Response {
    pub fd: OwnedFd,
}

pub async fn request(socket_addr: impl AsRef<Path>, req: Request) -> Result<Response, Error> {
    let socket = UnixSeqpacket::connect(socket_addr).await?;
    let mut buf = [0; 1024];
    bincode::encode_into_slice(&req, &mut buf, bincode::config::standard())?;
    let _ = socket.send(&buf).await?;

    let mut ancillary_buffer = [0; 128];
    let mut bufs = [IoSliceMut::new(&mut buf)];
    let (_read, ancillary) = socket
        .recv_vectored_with_ancillary(&mut bufs, &mut ancillary_buffer)
        .await?;

    // TODO read WireResponse

    if let Some(OwnedAncillaryMessage::FileDescriptors(mut fds)) = ancillary.into_messages().next()
    {
        if let Some(fd) = fds.next() {
            return Ok(Response { fd });
        }
    }
    Err(Error::Unknown)
}
