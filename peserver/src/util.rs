use bytes::{Bytes,BytesMut};
use std::net::{IpAddr,Ipv6Addr};

use pingora;
use pingora::proxy::Session;

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

fn ipv6_64(ip: &Ipv6Addr) -> [u8; 8] {
    let o = ip.octets();
    [o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]]
}

pub fn session_ip_id(session: &Session) -> u64 {
    let ip = session
        .client_addr()
        .and_then(|x| x.as_inet())
        .map(|x| x.ip());
    match ip {
        Some(IpAddr::V4(ipv4)) => u32::from_ne_bytes(ipv4.octets()) as u64,
        Some(IpAddr::V6(ipv6)) => u64::from_ne_bytes(ipv6_64(&ipv6)),
        None => 42,
    }
}

pub mod premade_errors {
    use once_cell::sync::Lazy;
    use pingora::protocols::http::error_resp;
    use pingora::http::ResponseHeader;
    use http::StatusCode;
    use crate::api::MAX_REQ_PER_SEC;

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

