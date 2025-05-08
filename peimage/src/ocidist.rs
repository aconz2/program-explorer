use std::io::Cursor;

use bytes::Bytes;
use log::trace;
use oci_spec::{
    distribution::Reference,
    image::{Digest, DigestAlgorithm, ImageConfiguration, ImageManifest},
};
use reqwest::{header, Method, StatusCode};
use sha2::Sha256;
use tokio::io::{AsyncWrite, AsyncWriteExt};

// common for blobs to get redirected
// we want to

const DOCKER_CONTENT_DIGEST_HEADER: &str = "docker-content-digest";

#[derive(Debug)]
pub enum Error {
    Reqwest(reqwest::Error),
    DigestMismatch,
    NoTagOrDigest,
    BothTagAndDigest,
    BadManifest,
    BadConfig,
    BadDockerContentDigest,
    Write,
    DigestAlgorithmNotHandled(DigestAlgorithm),
    StatusNotOk(StatusCode),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Error {
        Error::Reqwest(error)
    }
}

pub struct Client {
    client: reqwest::Client,
}

pub struct ManifestResponse {
    data: Bytes,
}

pub struct ConfigurationResponse {
    data: Bytes,
}

impl ManifestResponse {
    pub fn data(&self) -> &Bytes {
        &self.data
    }
    pub fn get(&self) -> Result<ImageManifest, Error> {
        ImageManifest::from_reader(Cursor::new(&self.data)).map_err(|_| Error::BadManifest)
    }
}

impl ConfigurationResponse {
    pub fn data(&self) -> &Bytes {
        &self.data
    }
    pub fn get(&self) -> Result<ImageConfiguration, Error> {
        ImageConfiguration::from_reader(Cursor::new(&self.data)).map_err(|_| Error::BadConfig)
    }
}

enum TagOrDigest<'a> {
    Tag(&'a str),
    Digest(&'a str),
}

impl<'a> TagOrDigest<'a> {
    fn try_from(r: &'a Reference) -> Result<Self, Error> {
        match (r.tag(), r.digest()) {
            (Some(tag), None) => Ok(TagOrDigest::Tag(tag)),
            (None, Some(digest)) => Ok(TagOrDigest::Digest(digest)),
            // from looking at the current code, this is unreachable as tag will get filled win
            // with latest
            (None, None) => Err(Error::NoTagOrDigest),
            // this is also not reachable I don't think
            (Some(_), Some(_)) => Err(Error::BothTagAndDigest),
        }
    }
    fn as_str(&'a self) -> &'a str {
        match self {
            Self::Tag(s) => s,
            Self::Digest(s) => s,
        }
    }
}

impl Client {
    pub fn new() -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(2))
            .build()?;
        Ok(Client { client })
    }

    pub async fn get_manifest(
        &mut self,
        reference: &Reference,
    ) -> Result<Option<ManifestResponse>, Error> {
        // handles converting docker.io to index.docker.io
        let domain = reference.resolve_registry();
        let repo = reference.repository();
        let td = TagOrDigest::try_from(reference)?;

        let url = format!("https://{domain}/v2/{repo}/manifests/{}", td.as_str());

        trace!("GET {url}");
        let request = self.client.request(Method::GET, &url)
            .header(header::ACCEPT, "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json");

        let response = request.send().await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {
                let digest = get_docker_content_digest(&response)?;
                let data = response.bytes().await?;
                check_data_matches_digest(digest.as_ref(), &data)?;
                Ok(Some(ManifestResponse { data }))
            }
            StatusCode::NOT_FOUND => Ok(None),
            status => Err(Error::StatusNotOk(status)),
        }
    }

    pub async fn get_image_config(
        &mut self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<Option<ConfigurationResponse>, Error> {
        let response = self.request_blob(reference, digest).await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {
                let expected_digest = get_docker_content_digest(&response)?;
                let data = response.bytes().await?;
                check_data_matches_digest(expected_digest.as_ref(), &data)?;
                Ok(Some(ConfigurationResponse { data }))
            }
            StatusCode::NOT_FOUND => Ok(None),
            status => Err(Error::StatusNotOk(status)),
        }
    }

    pub async fn get_blob(
        &mut self,
        reference: &Reference,
        digest: &Digest,
        writer: &mut (impl AsyncWrite + std::marker::Unpin),
    ) -> Result<Option<usize>, Error> {
        let mut response = self.request_blob(reference, digest).await?;
        trace!(
            "domain={:?} addr={:?}",
            response.url().domain(),
            response.remote_addr()
        );

        match response.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => {
                return Ok(None);
            }
            status => {
                return Err(Error::StatusNotOk(status));
            }
        }

        let mut len = 0;

        // how to be polymorphic over algo better?
        match digest.algorithm() {
            DigestAlgorithm::Sha256 => {
                use sha2::Digest;
                let mut hasher = Sha256::new();
                while let Some(chunk) = response.chunk().await? {
                    len += chunk.len();
                    hasher.update(&chunk);
                    writer.write_all(&chunk).await.map_err(|_| Error::Write)?;
                }
                check_digest_matches(digest, hasher)?;
            }
            algo => {
                return Err(Error::DigestAlgorithmNotHandled(algo.clone()));
            }
        };
        //println!("check {:?} {:?}", digest, hex::encode(&computed_digest));

        Ok(Some(len))
    }

    async fn request_blob(
        &mut self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<reqwest::Response, Error> {
        let domain = reference.resolve_registry();
        let repo = reference.repository();
        let url = format!(
            "https://{domain}/v2/{repo}/blobs/{}:{}",
            digest.algorithm().as_ref(),
            digest.digest()
        );
        trace!("GET {url}");
        Ok(self.client.request(Method::GET, &url).send().await?)
    }
}

fn get_docker_content_digest(response: &reqwest::Response) -> Result<Option<Digest>, Error> {
    response
        .headers()
        .get(DOCKER_CONTENT_DIGEST_HEADER)
        .map(|header_value| -> Result<Digest, Error> {
            header_value
                .to_str()
                .map_err(|_| Error::BadDockerContentDigest)?
                .try_into()
                .map_err(|_| Error::BadDockerContentDigest)
        })
        .transpose()
}

fn check_digest_matches(expected: &Digest, digest: impl sha2::Digest) -> Result<(), Error> {
    if digest_eq(expected.digest(), digest) {
        Ok(())
    } else {
        Err(Error::DigestMismatch)
    }
}

fn check_data_matches_digest(expected: Option<&Digest>, data: &[u8]) -> Result<(), Error> {
    if let Some(expected) = expected {
        if data_matches_digest(expected, data)? {
            Ok(())
        } else {
            Err(Error::DigestMismatch)
        }
    } else {
        Ok(())
    }
}

fn data_matches_digest(expected: &Digest, data: &[u8]) -> Result<bool, Error> {
    match expected.algorithm() {
        DigestAlgorithm::Sha256 => {
            use sha2::Digest;
            let mut hasher = Sha256::new();
            hasher.update(data);
            Ok(digest_eq(expected.digest(), hasher))
        }
        algo => Err(Error::DigestAlgorithmNotHandled(algo.clone())),
    }
}

// is this too weird? it checks without allocating
// oci::image::Digest guarantees the format of the digest string
fn digest_eq(digest_lower_hex_str: &str, digest: impl sha2::Digest) -> bool {
    let digest_bytes = digest.finalize();
    let l = digest_lower_hex_str.len();
    if l != 2 * digest_bytes.len() {
        return false;
    }
    let as_bytes = (0..l)
        .step_by(2)
        .map(|i| u8::from_str_radix(&digest_lower_hex_str[i..i + 2], 16).unwrap());
    // zip guaranteed to equal
    as_bytes.zip(digest_bytes.iter()).all(|(l, r)| l == *r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_digest_eq() {
        assert_eq!(
            true,
            digest_eq(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                [
                    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x1, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d,
                    0xae, 0x22, 0x23, 0xb0, 0x3, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10,
                    0xff, 0x61, 0xf2, 0x0, 0x15, 0xad
                ]
                .as_ref()
            )
        );
        assert_eq!(
            false,
            digest_eq(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ac",
                [
                    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x1, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d,
                    0xae, 0x22, 0x23, 0xb0, 0x3, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10,
                    0xff, 0x61, 0xf2, 0x0, 0x15, 0xad
                ]
                .as_ref()
            )
        );
        assert_eq!(
            false,
            digest_eq(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015a",
                [
                    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x1, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d,
                    0xae, 0x22, 0x23, 0xb0, 0x3, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10,
                    0xff, 0x61, 0xf2, 0x0, 0x15, 0xad
                ]
                .as_ref()
            )
        );
        assert_eq!(
            false,
            digest_eq(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                [
                    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x1, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d,
                    0xae, 0x22, 0x23, 0xb0, 0x3, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10,
                    0xff, 0x61, 0xf2, 0x0, 0x15
                ]
                .as_ref()
            )
        );
    }
}
