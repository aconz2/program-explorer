use std::net::{IpAddr,Ipv6Addr};

use bytes::{Bytes,BytesMut};
use http::{Response,StatusCode};
use serde::Serialize;
use sha2::{Sha256,Digest};
use base64::prelude::{BASE64_STANDARD,Engine};

use pingora;
use pingora::proxy::Session;
use pingora::protocols::http::ServerSession;

use crate::api::{APPLICATION_JSON,APPLICATION_X_PE_ARCHIVEV1};

pub async fn read_full_server_request_body(session: &mut ServerSession, max_len: usize) -> Result<Bytes, Box<pingora::Error>> {
    let mut acc = BytesMut::with_capacity(4096);
    loop {
        match session.read_request_body().await? {
            Some(bytes) => {
                acc.extend_from_slice(&bytes);
                if acc.len() > max_len {
                    return Err(pingora::Error::new(pingora::ErrorType::ReadError).into());
                }
            }
            None => {
                break;
            }
        }
    }
    Ok(acc.freeze())
}

pub async fn read_full_client_response_body(session: &mut pingora::protocols::http::v1::client::HttpSession) -> Result<Bytes, Box<pingora::Error>> {
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

pub fn response_no_body(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_LENGTH, 0)
        .body(vec![])
        .unwrap()
}

pub fn response_string(status: StatusCode, body: &str) -> Response<Vec<u8>> {
    let body = body.as_bytes().to_vec();
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        .unwrap()
}

pub fn response_json<T: Serialize>(status: StatusCode, body: T) -> serde_json::Result<Response<Vec<u8>>> {
    Ok(response_json_vec(status, serde_json::to_vec(&body)?))
}

pub fn response_json_vec(status: StatusCode, body: Vec<u8>) -> Response<Vec<u8>> {
    // TODO presize headermap
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, APPLICATION_JSON)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        .unwrap()
}

pub fn response_pearchivev1(status: StatusCode, body: Vec<u8>) -> Response<Vec<u8>> {
    // TODO presize headermap
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, APPLICATION_X_PE_ARCHIVEV1)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        .unwrap()
}

pub fn etag(data: &[u8]) -> String {
    let hash = Sha256::digest(&data);
    let mut ret = String::with_capacity(16);
    ret.push('W');
    ret.push('/');
    ret.push('"');
    BASE64_STANDARD.encode_string(hash, &mut ret);
    ret.push('"');
    ret
}


pub mod premade_responses {
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
    pub static PAYLOAD_TOO_LARGE: Lazy<ResponseHeader> = Lazy::new(|| error_resp::gen_error_response(StatusCode::PAYLOAD_TOO_LARGE.into()));

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

    pub static NOT_MODIFIED: Lazy<ResponseHeader> = Lazy::new(|| {
            let mut header = ResponseHeader::build(StatusCode::NOT_MODIFIED, Some(1)).unwrap();
            header.insert_header("Content-Length", "0").unwrap();
            header
    });
}

