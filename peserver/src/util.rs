use bytes::{Bytes,BytesMut};
use pingora;

pub async fn read_full_body(session: &mut pingora::protocols::http::v1::client::HttpSession) -> Result<Bytes, Box<pingora::Error>> {
    let mut acc = BytesMut::with_capacity(4096);
    loop {
        match session.read_body_ref().await? {
            Some(ref bytes) => {
                acc.extend_from_slice(&bytes);
            }
            None => {
                break;
            }
        }
    }
    Ok(acc.freeze())
}

